# Architecture

wlshare connects to a wlroots-based Wayland compositor through its public
protocols. It is one process with two halves and one shared framebuffer between them.

```text
wlroots compositor ── Wayland socket ──▶ compositor thread ──▶ Framebuffer ──▶ session tasks ──▶ TCP
                     screencopy           (calloop)            + damage log     (tokio)          RFB clients
                     image-copy-capture                       + cursor image
                     output-management                        ◀── Commands ◀──
                     virtual keyboard/pointer
                     data-control
```

- `crates/wlshare-rfb` decides every byte on the wire: handshake, message parsing
  and building, RSA-AES and its frames, the ZRLE encoder, the VP9 encoding, the
  density, outputs, camera and microphone extensions. It has no platform dependency and its tests decode every encoder's
  output with an independent decoder written from the RFC, and run the RSA-AES
  exchange against a client written from the specification.
- `crates/wlshare` is the daemon. `compositor.rs` is the Wayland thread and its
  command handler; `capture.rs`, `cursor.rs`, `outputs.rs`, `input.rs` and
  `clipboard.rs` are the protocols it speaks; `audio.rs`, `camera.rs` and
  `microphone.rs` are PipeWire's side, and `decode.rs` is the camera's libavcodec decoder;
  `framebuffer.rs` is the shared pixels and damage;
  `session.rs` is one client; `auth.rs` checks an RSA-AES login and `pam.rs` is
  the system half of that check; `shared.rs` is what crosses between them.
- `crates/wlshare-client` is the other end: the desktop client the macOS and
  Windows apps (`wlshare-macos`, `wlshare-windows`) are built on, everything
  but the window. See [The desktop client](#the-desktop-client).

## The desktop client

`wlshare-client` is the session the native apps run: the handshake, the
density and resize rules, VP9 or ZRLE decoded into a framebuffer and its
damage, the pointer's shape, the clipboard both ways and the sound into a
playback buffer — the daemon's `session.rs` read from the other end, every
byte of it through `wlshare-rfb`. `Client::connect` runs it on a thread of its
own; the window reads pixels, the cursor, the clipboard and the sound under
short locks and posts input as commands.

Each app keeps what is about its platform in a small core of its own on top of
this crate: the table that turns its key events into keysyms, the gathering of
its scrolls into wheel notches, and the C ABI its app calls in through — a
static library and a C header on macOS, a DLL and P/Invoke on Windows. Those
cores depend on `wlshare-client` by this repository's release tag, and it
re-exports `wlshare-rfb`, so an app moves one tag. A change to how a client
behaves on the wire is made here once, for both, and reaches them as a release.

Like `wlshare-rfb` it has no platform dependency and a bare `cargo test` covers
it; its live tests (`tests/live_session.rs`, ignored by default) talk to a
real wlshare.

## One session at a time

The desktop belongs to one client. A connection that finishes the handshake
takes it, and the client that held it is disconnected with a message naming the
one that took over — the same trade Windows Remote Desktop makes, and the reason
the takeover happens *after* the handshake: an unauthenticated connection, or
one whose login is refused, never displaces the session in progress. RFB's ClientInit
shared flag is read and dropped; there is no configuration for it and no way to
watch alongside somebody else.

The compositor thread holds the single client id, so the rule is one comparison:
input, resize, density, output selection and clipboard from anyone else are
dropped, which is what
a superseded session's last in-flight messages are. Taking over releases the
keys and buttons the previous client held and its pending resize, and capture
runs from the first handshake until the client on the desktop leaves.

Who holds it is a `watch` and not one of the broadcast events: a session slow
enough to lag the broadcast drops events, and dropping this one would leave two
clients on the desktop. A watch keeps only the latest value, so the superseded
session ends on the value it finds there — and is ignored until it does.

The watch races the whole session, not the gaps in it. A session runs its
message loop against the takeover in one `select!`, so a client that has stopped
reading its socket is cut off in the middle of the write that is blocking on it,
rather than holding its capture and its task open for as long as it refuses to
read. Client ids only ever go up, which is what lets a session decide by
comparison instead of by acknowledgement: an active id above its own is a
connection that joined after it, whether or not it ever saw itself there. That
is the answer to the other end of the race — two connections whose joins are
queued together, where the second is on the desktop before the first has
subscribed at all.

## Capture

wlr-screencopy `copy_with_damage` into a `wl_shm` buffer, one frame in flight,
paced by `max_fps`. The compositor answers a damage-only copy only when
something changed, so an idle desktop costs nothing. Damaged rectangles are
copied into the framebuffer under its lock, the generation counter advances,
and every session is woken through a `watch`.

A framebuffer holding no pixels yet is the exception, both ways round. It is
asked for a plain `copy` rather than a damage-only one, because a damage-only
copy is answered only when the output changes and an output nobody is touching
may not change for minutes -- which would leave a client on a blank screen, or
on the last picture of the output it just left, until somebody moved the mouse.
And the frame that comes back is taken whole, whatever the compositor reported
changed: damage is measured against the frame before, and a framebuffer just
made, just resized, or just pointed at another output has no frame before, so
copying only the reported rectangles would leave the rest of it blank. Both
follow from `painted`, which the framebuffer clears on every resize.

The framebuffer keeps a log of `(generation, rect)`. A session asks for the
damage after the generation it last sent and gets the merged union; a session
behind the log, or one that has seen nothing yet, gets the whole framebuffer.
Sessions copy the pixels they need out under the lock and encode after releasing
it, so encoding a slow client's update never holds up a capture.

### The cursor

The pointer is excluded from the frame (`overlay_cursor = 0`). On a headless
output this needs wlroots 0.19 or newer, whose headless backend keeps cursors on
a distinct plane instead of painting them permanently into the output. The same
wlroots exports that plane as the pointer cursor of the output's
ext-image-capture-source, and `cursor.rs` holds an ext-image-copy-capture cursor
session on it while a client is on the desktop: its frames are the cursor image
the application under the pointer chose, in the output's pixels, which are the
framebuffer's. One frame is always in flight, and wlroots answers it only when
the cursor buffer changes — a new shape, a new scale — so a pointer that only
moves costs nothing. The session's `enter` and `leave` say whether the cursor is
on the shared output and showing; outside them there is no image. A session
needs a `wl_pointer`, which the seat refuses before it has ever had one and
turns inert when it loses one, so the session waits for the seat to name a
pointer (wlshare's own virtual pointer is one) and takes a fresh `wl_pointer`
each time it opens: on every output switch, where retargeting the virtual
pointer briefly takes the seat's pointer away.

The image is cropped to the pixels it paints and its hotspot, and goes to every
client in an update of its own whenever it changes. Every client must advertise
the standard Cursor pseudo-encoding (`-239`) before it asks for framebuffer
pixels; one that also lists Cursor With Alpha (`-314`) is sent the premultiplied
RGBA as it is, Raw-encoded, and the rest get pixels in their own format beside a
mask cut at half alpha, which loses a shadow and antialiased edges. No image — a
hidden pointer, or one on another output — is an empty rectangle. RFB cursor
dimensions are framebuffer pixels, as the captured image is, so it is sent at its
own size. The client positions it at the coordinates it already sends in pointer
events. Cursor motion therefore never waits for a captured frame.

Anything that locks the output to software cursors — another client
screencopying with the cursor painted in, such as `grim -c` — takes the cursor
off its plane. wlroots then paints it into every capture of that output, this
one's included, and the cursor session reports it gone.

### Frame layout

The framebuffer is always XRGB8888 with its rows top down, but a captured frame
need not be either, so `FrameLayout` records how the frame in flight differs and
the copy straightens it out. Both differences come from the compositor rather
than from anything asked of it: y-invert is reported per frame, and the single
shm format offered is whatever the renderer prefers to read back. So a
compositor on an Intel iGPU hands over red and blue the other way round from one
compositing in software, and only this copy knows it: past it a frame is
XRGB8888, rows top down, and the encoders need no cases.

`FrameLayout::bytes` is a permutation, not a conversion -- where each of the
framebuffer's four bytes sits in the frame's own pixel -- so what it can absorb
is every 32-bit order at eight bits a channel and nothing else:

| offered by the compositor | in memory  | from |
| ------------------------- | ---------- | ---- |
| `XRGB8888`, `ARGB8888`    | `B G R X`  | pixman, GLES2, Vulkan |
| `XBGR8888`, `ABGR8888`    | `R G B X`  | pixman, GLES2, Vulkan |
| `RGBX8888`, `RGBA8888`    | `X B G R`  | pixman |
| `BGRX8888`, `BGRA8888`    | `X R G B`  | pixman |

That is the whole of what wlroots' screencopy can offer for a desktop: its
GLES2 renderer resolves `GL_IMPLEMENTATION_COLOR_READ_FORMAT` to one of the
first four, its Vulkan renderer reports the texture's own format, and its pixman
renderer reports the output texture's, which adds the four with the unused byte
first.

#### What is deliberately not handled

wlroots can in principle report formats outside that table, and each would need
a real conversion into the framebuffer's eight bits a channel rather than a
rearrangement of bytes:

- **10-bit** (`XRGB2101010` and its seven siblings). Reachable on a deep-colour
  output; the one entry here with a plausible future. It needs the channels
  narrowed, and the honest version of that is its own path, not a widening of
  the permutation.
- **Packed 24-bit** (`RGB888`, `BGR888`) at three bytes per pixel, which the
  four-byte stride arithmetic in `Framebuffer::apply` assumes away.
- **16-bit** (`RGBA4444` and its siblings), which only very old GLES drivers
  report.
- **`RGB565`, `BGR565`, the 5551 family, and `XBGR16161616`(`F`)**. wlroots can
  report these, but neatvnc has no case for them either, so they are not a gap
  against wayvnc so much as a gap in every wlroots VNC server.

None of these was reachable from Sway in the measured ordinary desktop, and an
unhandled format is not silent: the capture logs what was offered and what was
wanted, and retries rather than serving a frozen picture.

## Sending pixels

A client gets an update when it has asked (`FramebufferUpdateRequest`, or once
for all with continuous updates) and there is damage. Each update is one
`FramebufferUpdate` of merged rectangles, at most 32, ZRLE-encoded on the
client's own deflate stream, or Raw before the client's first `SetEncodings` and
for a client that never lists ZRLE — or, for a client that lists the VP9
encoding, one rectangle of the whole framebuffer ([below](#the-vp9-encoding)).

With Fence negotiated, every update ends with a fence the client echoes, and the
next update waits for the echo. One update is in flight at a time, so a slow
link is never flooded and frames coalesce in the framebuffer meanwhile. remotex
negotiates both ContinuousUpdates and Fence.

What the echo means is the client's to decide, and `wlshare-client` holds it
until its *window* has taken the frame rather than answering from the decoder
([below](#the-desktop-clients-paint)).

A size change goes out first, as its own update — an ExtendedDesktopSize
rectangle whose reason says who asked (the server, this client, another
client), or a DesktopSize rectangle for a client without the extension — and the
whole framebuffer follows in the next update. A client that negotiated neither
cannot be told and is disconnected at its next update rather than sent pixels at
a size it does not know. The ExtendedDesktopSize announcement that answers the pseudo-encoding is an
update too, and waits for a request like any other.

## The VP9 encoding

A private encoding, `WLSV` (`0x574c5356`), for wlshare's own desktop clients
and the remotex gateway: the whole desktop as one VP9 stream, for a client that
would rather have a picture that moves than one that is exact. remotex lists it
for a browser that decodes 4:4:4 and passes each frame to the browser as it
came, since it is the stream remotex would otherwise encode from ZRLE's pixels;
for a browser that does not, it lists ZRLE and encodes 4:2:0 itself. No other VNC
client lists it, so nothing changes for them.

A client that lists it gets it instead of ZRLE, wherever in the list it is. Each
update is then one rectangle covering the whole framebuffer, whose body is a
length word and one VP9 frame:

```text
u32 length   the frame's bytes
u8[length]   one VP9 frame
```

Successive rectangles are one stream, each frame coded against the ones before
it, so a client decodes them all with one decoder, in order. What it holds is
fixed:

- **8-bit 4:4:4, VP9 profile 1.** A colour sample per pixel: the loss 4:2:0
  costs a desktop is its text's colour — a one-pixel coloured stem shares its
  sample with three pixels of background — and no quantizer puts it back.
- **BT.601 at studio swing**, converted from the framebuffer's `B, G, R, X` and
  declared in the keyframe header, so a decoder converts back with the same
  matrix. The client's pixel format does not apply.
- **A quantizer and a frame rate that follow the link.** The 1–100 dial maps
  onto VP9's 8–63, finest last, as remotex's dial does; rate control is pinned
  to wherever the dial is, with no bitrate, no adaptive quantization and no
  dropped frames. A session starts at `vp9_quality` (90 by default, for the
  LAN wlshare mostly runs on), which is a ceiling it never goes above, and
  walks down to a floor of 20 while the client is behind — remotex's walk
  (`crates/wlshare/src/quality.rs`). The floor is a constant, not a key, as
  every adaptive stream's is: where WebRTC's quality scaler hands off from the
  quantizer to resolution and frame rate at its own threshold, this walk hands
  off to the frame rate, and the settle below sharpens a quiet desktop back at
  `vp9_quality`.
  A frame's queueing is its fence's round trip — answered once the client has
  the frame, which for wlshare's own client means once its window has drawn it
  ([below](#the-desktop-clients-paint)) — less the shortest of the last
  minute's, so distance does not read as queueing; a keyframe counts towards
  that floor but is no verdict. Without Fence it is how long writing the frame blocked. Two
  behind frames (60 ms of queueing or more) among the last four give quality
  up: ten points, twenty at 150 ms, thirty at 400 ms; once the dial is on the
  floor the capture's frame interval doubles instead, 33 ms up to 133 at the
  default `max_fps`, and at 400 ms both go at once. The cursor, a resize and
  the audio announcement never wait for a slowed frame's turn. A step is taken at most once a second, and while the lag
  is still falling a fifth per second from the step before — the queue it
  left draining — no further step is taken, since the step was enough; after
  a keyframe the verdicts wait two seconds, since the frames behind it queue
  behind its crossing. A second of clear frames (30 ms or less), four at the
  least, takes frames back first, then quality in steps that double from
  three to twenty-four while the link keeps taking them; a step the link
  refuses within four seconds is walked back after 300 ms to the quality it
  came from, and for fifteen seconds the walk climbs no further than halfway
  back towards the one refused. The dial
  moves on the running encoder, so a move costs no keyframe, and an encoder
  made at a new size starts where it stands. A slowed session paces its own
  frames; an unslowed one is paced by the capture and its one fence in flight.
  Screen-content tuning, libvpx's
  realtime speed 7, no lag, and threads with row and tile parallelism: the
  machine's cores less two, at most eight, for the encoder, since an encode
  is a burst the person at the other end waits on, and libvpx clamps the tile
  columns to what the width allows; half the machine, at most four, for the
  decoder, which gains nothing past the stream's tiles. The conversion
  between the framebuffer's pixels and the planes is the `yuv` crate's, on
  the AVX2 or NEON path the machine has: a scalar loop over a 4K frame was a
  fifth of an encode and a third of a decode.
- **A quiet desktop is sharpened at `vp9_quality`, and the walk keeps its
  place.** The walk only runs when a frame goes out, and a frame only goes out
  when something changed, so a desktop that stops right after the link
  coarsened it would keep that picture until it changed again. Once a frame
  encoded below `vp9_quality` has been delivered — its fence answered, or
  without Fence its write finished — and nothing has been sent for 500 ms
  since, the unchanged picture goes out again, at the next update the client
  asks for, as one inter frame at `vp9_quality`: libvpx codes the residual of
  unchanged blocks at the finer quantizer, so it sharpens the whole desktop
  without a keyframe
  (`a_finer_quantizer_sharpens_an_unchanged_picture_without_a_keyframe`
  guards that). The encoder is retuned for that one frame and returned to the
  walk's quality after it, and the frame is no verdict: the screen stopping
  says nothing about the link, and a walk that started every burst of motion
  from the ceiling was measured to put the picture half a second behind at
  every one. The clear frames before the quiet do not span it either: the
  walk's run of clear frames starts over at the settle, so a burst earns its
  step back up from its own frames rather than taking one on its first. A
  frame that went out at `vp9_quality` owes nothing, and a desktop that goes
  quiet after one sends nothing. remotex's settle for its own whole-desktop
  streams.
- **Keyframes only when a decoder needs one**: the first frame after the
  encoding is listed, the first at a new size (the encoder is made again for
  it), and the frame that answers a non-incremental request. There is no
  periodic keyframe; nothing is lost on TCP.

An update is sent when anything is damaged, and the frame is the whole picture:
the encoder's inter-frame coding is what makes an unchanged region cost nothing.
The encode runs on the session's worker, which is told it is blocking, and the
fence keeps one frame in flight as it does any update. A `SetEncodings` that
drops the encoding is answered with the whole framebuffer in ZRLE, since the
client is holding a lossy picture.

libvpx comes from `libvpx-prebuilt`'s static archive, behind the crate's
`encode` and `decode` features.

### The desktop client's paint

The walk above is only as honest as the fence it times, and a fence echoed the
moment a frame is decoded times the link and the decoder and nothing else. A
window that cannot upload a 4K frame as fast as the desktop draws one would
read as a healthy client: the server would keep coding at the dial, the window
would keep overwriting frames it never showed, and the person would watch a
picture that lags while every measurement said the link was clear.

So `wlshare-client` holds the echo until the window has taken the pixels —
`Client::with_frame` handing back damage is the acknowledgment, the same shape
as the `paintAck` remotex's browser sends after its painter draws. The whole
path a frame takes to the screen is then inside the round trip the walk reads,
and, because the server sends nothing until the echo arrives, a frame the
window would only have overwritten unseen is never encoded at all.

A window that is not drawing must not be able to stop the desktop, so the echo
goes out unheld after 500 ms — `PAINT_GRACE`, remotex's `PAINT_WINDOW_GRACE` —
and the window is then counted as one that is not drawing: the fences after it
are echoed at once, and the desktop keeps streaming at whatever the link bears
until the first paint puts the window back in the loop. A hidden window costs
the session nothing and never drags the quality down with it.

## The density extension

Standard RFB has no word for pixel density. The extension is one pseudo-encoding,
`0x574c5348` (`WLSH`), and one message type, `0xE0`, in both directions; scales
are 16.16 unsigned fixed point.

- **OutputScale**, server → client, ten bytes: type, padding, width and height
  in pixels, scale. Sent as the answer to *every* `SetEncodings` that lists the
  pseudo-encoding — the only way support is announced — and whenever the shared
  output's scale or mode changes, before the frame at the new size is captured,
  so the report precedes the resize rectangle.
- **ClientDensity**, client → server, ten bytes in OutputScale's layout: type,
  padding, width and height in pixels, scale. The client states the scale it
  wants the output drawn at *and* the size it wants at that scale, every time,
  so a change of density is one output configuration: a scale alone would change
  the logical size until a resize followed, and every application would redraw
  twice. A resize at an unchanged density is `SetDesktopSize`. Honoured only from
  a client that listed the pseudo-encoding and ExtendedDesktopSize, only in the
  range 0.5–8 and at a size that is not empty. The server sets the output's mode
  and scale through wlr-output-management in one configuration, asking only for
  what differs, under the same rules as a resize — a headless output and
  `resize = true` — and **answers every declaration** with an OutputScale: after
  the compositor's head change, or at once with the output as it is when the
  declaration matches, is refused, or cannot be applied. A configuration the
  compositor accepts without changing the scale is answered too: `succeeded` is
  followed by one `wl_display.sync` round trip, after which the output is
  reported as it is if no head change arrived. One declaration's configuration
  is out at a time, and its events are told from any other's, so each is
  answered once: one arriving meanwhile waits for it to settle, and a newer one
  replaces it, the replaced one answered with the output as it is. A new size
  reaches the client as
  an ExtendedDesktopSize rectangle whose reason is this client, as a
  SetDesktopSize's does.

The exact scale comes from the wlr-output-management head, fractional included;
`wl_output.scale`, which wlroots rounds up, is the fallback when the protocol is
absent.

## The outputs extension

One framebuffer is one output, so a desktop with two monitors has to be asked
which one to send. Standard RFB has no word for that either — `ExtendedDesktopSize`
describes screens *inside* one framebuffer — so this is a second private
extension in the shape of the first: one pseudo-encoding, `0x574c534f` (`WLSO`),
and one message type, `0xE1`, in both directions.

- **OutputList**, server → client: type, padding, a count, the shared output's
  id, then an entry per output — id, width and height in pixels, scale as 16.16
  fixed point, a flags byte whose bit 0 says the output is headless, and a
  length-prefixed UTF-8 name. Sent as the answer to *every* `SetEncodings` that
  lists the pseudo-encoding — the only way support is announced — and again
  whenever the list, an entry, or the shared output changes. Entries are ordered
  by name, and an output whose name or mode has not arrived yet is not in them.
  The id is the `wl_output` global, unique for as long as the output exists.
- **SelectOutput**, client → server, eight bytes: type, three bytes of padding,
  the id of the output to share. Honoured only from a client that listed the
  pseudo-encoding and holds the desktop, and **answered with an OutputList**
  either way: a request naming an output the compositor no longer has, or the one
  already shared, is answered with the list as it is. So a client's menu follows
  what is on the canvas rather than what was clicked.

A switch stops the capture, points the virtual pointer at the new output —
`zwlr_virtual_pointer` takes its output when it is made and never again, so what
the client holds is let go and the pointer is remade — takes the new size into
the framebuffer blank, reports the geometry, and starts capturing again. The
client is sent nothing until a frame of the output it asked for has arrived: a
different size reaches it as an ExtendedDesktopSize rectangle with the server as
the reason, and a same-sized output as a full repaint. Which output is shared to
begin with is `output` in the configuration, or the first one.

An id is selectable exactly when it is listable: the output's properties have
arrived, it has a name, and a capture of it would produce pixels. One rule serves
both, so a client can never name something the list would not have shown it, and
an output still arriving cannot become a desktop of no size.

An output the compositor takes away while it is the shared one is not left as a
name standing for nothing — that would stop the capture with nothing left to
start it again, and a client would sit watching a picture that had quietly
stopped changing. The desktop moves to whatever the list shows first, through the
same sequence a client's own switch runs, so the client is told the new geometry
and sent the new output's pixels. With no output left the capture stops and the
list goes out empty, the last geometry and the last picture standing until an
output appears; the first one to arrive is adopted the same way.

Only a headless output is ever resized or rescaled, so switching to a real
monitor leaves a client's resize and density requests answered *prohibited* —
that monitor's mode belongs to the person sitting at it.

The choice outlives the client that made it: the next connection opens on the
output the last one asked for, not on the configured default, until the daemon
restarts. Measured on a two-monitor sway session in
[remotex's `docs/wlshare-outputs.md`](https://github.com/andrewtheguy/remotex/blob/main/docs/wlshare-outputs.md),
which is where the gateway's half of this lives.

## The audio extension

The desktop's sound, as FLAC, on the connection the pixels use. It is private —
pseudo-encoding `0x574c5346` (`WLSF`) and server message type `0xE4` — and its
clients are the remotex gateway and the macOS viewer. The client's messages and the stream's
begin and end are borrowed from the QEMU Audio extension `rfbproto` registers,
message type `255` submessage `1`; QEMU's pseudo-encoding, `-259`, is not
spoken, because what it promises is raw samples and none are sent. A client that
lists only `-259`, gtk-vnc for one, hears nothing.

- **The announcement**, server → client: an empty pseudo-rectangle of encoding
  `WLSF` in a `FramebufferUpdate` of its own, sent ahead of any pixels to a
  client whose `SetEncodings` listed it. The only way support is announced.
- **Set format, enable, disable**, client → server, QEMU's messages: the sample
  format, channel count and frequency are the client's to choose, and the
  server converts what the desktop plays into them. The formats are QEMU's
  codes 0–3, U8, S8, U16 and S16; its 32-bit codes are refused, because FLAC
  stores at most 24 bits. The frequency is bounded at 8 kHz, below which a
  frame is shorter than the encoder takes, and at 96 kHz, flacenc's own
  ceiling. A code or rate outside those is fatal.
- **Begin and end**, server → client, QEMU's messages.
- **A FLAC frame**, server → client, between a begin and an end:

| Offset | Type | Field |
|---|---|---|
| 0 | U8 | message type, `0xE4` |
| 1 | U8[3] | padding |
| 4 | U32 | length of the frame |
| 8 | U8[] | one FLAC frame |

Every frame holds exactly `frequency / 50` frames of samples, rounded down —
twenty milliseconds, 960 at 48 kHz — in fixed-blocking mode, numbered from zero
at each begin. The FLAC stream header, `STREAMINFO`, is never sent: everything
in it is already agreed, so a client builds it from the format it set, with
that block size as both minimum and maximum. An unsigned format has the top
bit of every sample flipped before it is encoded, which maps its range onto the
signed one of the same width with silence on zero; the client flips it back.
Decoded samples are interleaved and little-endian, and bit for bit what the
capture produced.

FLAC is lossless, so the gateway's Opus encode stays the only lossy step, while
music and speech cost about two-thirds of their 1.5 Mbit/s PCM rate or less and
a silent desktop a few bytes a frame. `wlshare-rfb` encodes with `flacenc`, and
decodes with symphonia's decoder, which shares nothing with it — the client's
half, `audio::FlacDecoder` beside `audio::streaminfo`, is what the encoder's
tests read every frame back with. The encoder is behind the crate's `encode`
feature and the decoder behind `decode`: the daemon turns on the one, a client
the other. A frame length past 64 KiB is fatal to a
client: the largest block there is, 20 ms of 16-bit stereo at 96 kHz, is 7680
bytes before compression.

While any client listens the host is silent, the way a remote desktop's sound
is: the desktop plays into the **speaker**, a sink of wlshare's own, rather than
into the host's. `audio.rs` makes it with the first client's enable and removes
it with the last one's disable or disconnect — a `support.null-audio-sink` named
`wlshare-speaker`, described as "wlshare remote audio", on a thread of its own.
Nothing on the host is changed to get there, no sink muted and no default
written. The speaker's `priority.session` is 100000, and WirePlumber makes the
available sink with the highest priority the default, after adding 30000 to the
one the user configured and up to 20000 to those configured before it; a
hardware sink's own is in the low thousands, so the speaker is the default
while it exists, chosen sink or not, and every stream that follows the default
moves to it. The node belongs to the speaker thread's PipeWire connection, so
when the thread quits or the daemon dies PipeWire removes it, WirePlumber makes
the host's sink the default again, and the streams follow it back. A stream an
application pinned to a sink of its own stays there and is heard on the host.

`audio.rs` starts one PipeWire capture per client that enables audio, on a
thread of its own. It is a `Stream/Input/Audio` node with
`stream.capture.sink = "true"` and `target.object` the speaker, which connects
it to the **speaker's monitor** — what the desktop is playing, whatever is
playing it — and `node.latency` asks for 20 ms buffers. The process callback runs on that
thread's loop and not on the graph's real-time one — `RT_PROCESS` is
deliberately not set, because the callback encodes, allocates, takes a mutex
and wakes a task, and doing any of that on the data thread could stall the whole
audio graph and give every application on the host an xrun. Encoding there
keeps it off the session's task, which has pixels to compress; a 20 ms buffer
takes a fraction of a millisecond. The encoder keeps what does not fill a frame
for the next buffer — PipeWire honours its own quantum before settling on the
requested one, so the first buffers of a session are often shorter than 20 ms —
and queues each frame it completes in a sixteen-deep queue, dropping the oldest
when a client cannot keep up: each FLAC frame decodes on its own, so a dropped
one is a 20 ms hole, and a stalled capture callback is worse. A set-format on a
running stream restarts the capture in the new format, holding the speaker
across so the host is not heard between the two captures, and a disable or a
disconnect stops it; what is left of a frame goes with it.

The session drains that queue before every framebuffer update, so sound is never
held behind a ZRLE frame it was ready before. A headless session needs no sink
of its own: the speaker is one.

The announcement is off unless the configuration sets `audio = true`, and while
it is, a client that lists the pseudo-encoding is told nothing.

## The camera extension

A client's camera, lent to the desktop. RFB carries nothing from a client but
input and a clipboard, and no registered extension carries video that way, so
this is a third private pair in the shape of the density and outputs extensions:
pseudo-encoding `0x574c5343` (`WLSC`) and message type `0xE2`, in both
directions. Every message is the type, an operation, two more bytes, and what
the operation carries; integers are big-endian.

| Direction | Operation | Bytes 2–3 | Then |
| --------- | --------- | --------- | ---- |
| client → server | 0, plug | padding | `u16` width, `u16` height, `u32` frame-rate numerator, `u32` denominator |
| client → server | 1, unplug | padding | nothing |
| client → server | 2, sample | flags (bit 0: keyframe), padding | `u32` length, one Annex B access unit |
| server → client | 0, available | padding | nothing |
| server → client | 1, start | padding | the plugged format, as a plug lays it out |
| server → client | 2, stop | padding | nothing |
| server → client | 3, keyframe | padding | nothing |

- **Available** answers *every* `SetEncodings` that lists the pseudo-encoding —
  the only way support is announced.
- **Plug** makes a camera of the H.264 the client will send; another plug
  replaces it, and an unplug or the client leaving removes it. A plug with no
  pixels or no rate, and a sample over 4 MiB, are fatal: they are a client that
  means something else by the fields. A plug past 4096x2304 pixels — H.264 level
  5.2's largest frame — is refused, logged, and leaves the client without a
  camera: the fields reach 65535x65535, whose pictures no buffer should hold.
- **Start** and **stop** are the desktop's decisions, not the client's: an
  application opened the camera, or the last one closed it. The client sends
  samples between the two and nothing outside them, and a stream opens on a
  keyframe.
- **Keyframe** is owed after a gap: H.264 cannot be decoded across a lost unit.

`camera.rs` makes each plugged camera a PipeWire node, `wlshare-camera-<client>`,
of class `Video/Source` and role `Camera`, described as "wlshare remote camera"
so it is not mistaken for a camera of the host's own. Like an audio capture it is
a thread of its own running PipeWire's loop, and its callbacks run on that loop,
not on the graph's real-time thread, because they decode and copy. It offers one
format — I420 at the plugged geometry and rate, what one decoder behind it makes
— and an application that wants another converts or does not open it. The node is
its own driver: nothing else in the graph knows when a camera frame is due, so
each decoded picture triggers the cycle that delivers it. The stream going to
*streaming* when an application links to it, and back when the last one leaves,
is what the session sends as start and stop.

`decode.rs` is the system's libavcodec, reached through `ffmpeg-sys-next` with
avcodec alone and linked dynamically, so the package depends on Debian's
`libavcodec` rather than carrying a codec. The decoder runs on one thread with
`LOW_DELAY`, so a unit is a picture the moment it is decoded rather than a frame
later. It takes 4:2:0 at eight bits, which is everything Constrained Baseline
makes. A picture at another size than the plug named is one the node cannot
offer, and ends the camera: it is said once, nothing more is decoded, the client
is sent stop, and the camera is unplugged.

Samples reach the camera thread through a queue eight deep. One that finds it full
is dropped, and so is every sample after it until a keyframe, which is asked of the
client once per gap — again if the keyframe itself found no room. A unit the
decoder refuses owes a keyframe the same way. Nothing here waits on the client's
socket: a desktop that stops watching costs the client nothing but a stop.

Measured 2026-09-14 on a sway session with PipeWire 1.4.2: a client plugging
640x480 at 15/1 and sending libx264 Constrained Baseline, and a PipeWire consumer
linking to `wlshare-camera-1` and offering YUY2, I420, NV12 and BGRx. The consumer
negotiated I420 640x480 at 15/1 and went to *streaming*, wlshare sent start, and
the consumer took whole pictures — 460800 bytes, stride 640 — until it left and
wlshare sent stop.

An application finds the node through PipeWire, and a browser through
xdg-desktop-portal's Camera interface, which hands it a PipeWire remote that
shows only nodes of role `Camera`. The portal exports that interface only when a
backend implements Access, to ask the user; `xdg-desktop-portal-wlr` does not,
and `xdg-desktop-portal-gtk` does. The portal finds its backends when it starts,
so a running one must be restarted after a backend is installed. Chrome also
reaches cameras through PipeWire only with
`chrome://flags/#enable-webrtc-pipewire-camera` enabled; without the flag, or
without the portal's Camera interface, it looks at `/dev/video*` alone and lists
no camera. Checked 2026-09-13 with Chrome 153 on a labwc session with
xdg-desktop-portal 1.20.3: with only `xdg-desktop-portal-wlr` installed, the
portal exported no `org.freedesktop.portal.Camera`, and with
`xdg-desktop-portal-gtk` added and the portal restarted, it did.

The announcement is off unless the configuration sets `camera = true`, and while
it is, a client that lists the pseudo-encoding is told nothing.

## The microphone extension

A client's microphone, lent to the desktop — the camera's twin, and what an RDP
host gets from MS-RDPEAI. The QEMU Audio extension carries sound one way only, so
this is a fourth private pair: pseudo-encoding `0x574c534d` (`WLSM`) and message
type `0xE3`, in both directions, every message the type, an operation, two more
bytes, and what the operation carries; integers are big-endian.

| Direction | Operation | Bytes 2–3 | Then |
| --------- | --------- | --------- | ---- |
| client → server | 0, plug | padding | nothing |
| client → server | 1, unplug | padding | nothing |
| client → server | 2, sample | padding | `u32` length, interleaved PCM |
| server → client | 0, available | padding | nothing |
| server → client | 1, start | padding | `u16` channels, `u16` padding, `u32` frequency |
| server → client | 2, stop | padding | nothing |

- **Available** answers *every* `SetEncodings` that lists the pseudo-encoding —
  the only way support is announced.
- **Plug** makes a microphone; another plug replaces it, and an unplug or the
  client leaving removes it. A sample over 256 KiB is fatal, and so is one that
  is not whole frames of the format the start named.
- **Start** and **stop** are the desktop's decisions: an application started
  recording, or the last one stopped. The client sends samples between the two
  and nothing outside them. The format is the server's, as a host's is over RDP:
  samples are always signed 16-bit little-endian, and the start names the channel
  count and rate. wlshare names mono at 48 kHz, which is what the remotex
  gateway's Opus decodes to, so nothing between the browser and the node
  resamples. There is no keyframe: a lost sample is a moment of silence.

`microphone.rs` makes each plugged microphone a PipeWire node,
`wlshare-microphone-<client>`, of class `Audio/Source` and role `Communication`,
described as "wlshare remote microphone". Like the camera it is a thread of its
own running PipeWire's loop, its callbacks on that loop and not on the graph's
real-time thread, and the stream going to *streaming* and back is what the
session sends as start and stop. It is not connected with `AUTOCONNECT`: a
source is linked to by what records from it, and linked on its own it would play
the client's voice into the default sink.

Unlike the camera it is not its own driver. Audio already has a clock, the
graph's, and a source keeping its own would drift against the sinks the
recording application also plays into; so the graph asks for a quantum when it
wants one and the node answers from a jitter buffer between the client's pace
and the graph's. The buffer holds back 60 ms before it plays — after a start, and
again after it runs dry — so a late sample is a gap in the stream rather than a
click in every quantum, answers silence while it has nothing, and keeps at most
200 ms, dropping the oldest, so a client that bursts after a stall is heard live
rather than late. A stop empties it.

Measured 2026-09-14 on a labwc session with PipeWire 1.4.2: a client plugging a
microphone and, on start, sending a 440 Hz tone 20 ms at a time, and `pw-record
--target wlshare-microphone-1` recording mono 48 kHz for three seconds and then
two. Each recording sent start as it linked and stop as it left; past the first
half second both held the tone at 440 Hz with no silent 10 ms block, and the node
was gone from the graph after the unplug.

The announcement is off unless the configuration sets `microphone = true`, and
while it is, a client that lists the pseudo-encoding is told nothing.

## Resize

`SetDesktopSize` sets a custom mode on the shared output, with the same rules.
A request that arrives with `resize = false` is answered *prohibited*. A request
for the current size is answered OK at once; a size the compositor rejects is
answered *invalid layout*; a request the compositor accepts is answered when the
frame at that size arrives, with an ExtendedDesktopSize rectangle naming this
client as the reason. Only a headless output — one named `HEADLESS-*` — is ever
reconfigured.

## Input and clipboard

Key events carry X11 keysyms. The server compiles the configured XKB keymap,
uploads it to the virtual keyboard, and searches the same keymap for a keycode
producing each keysym, preferring the lowest shift level. Modifier state is
tracked with `xkb_state` and sent after every key. A character keysym names a
character the client has already cased — remotex never forwards Caps Lock and
sends `A` or `a` as the browser resolved it — so before each press the server
checks what the keycode would produce under the current modifiers, and presses
Shift or lets a held Shift go around the key when the keycode alone would type
the other case. A keysym that names a key rather than a printable character is
exempt and goes out on its keycode under whatever the client holds: Shift+Tab
arrives as Shift then `Tab`, the keycode's shifted level is `ISO_Left_Tab`, and
letting Shift go to make it produce `Tab` would type a plain Tab.
Keys and buttons are let go when the client leaves or is superseded, and a
connection that never finished the handshake releases nothing. Pointer events
arrive in framebuffer pixels and are injected as absolute positions against the
framebuffer's extent, which the virtual pointer maps onto the shared output.
Wheel "buttons" become discrete axis events.

The clipboard is shared as UTF-8 text through Extended Clipboard, and only
that way: latin-1 cut text is dropped in both directions, and a client that
does not list the extension has no clipboard. Every `SetEncodings` listing it is
answered with the server's caps — text, every action, and no unsolicited text,
as the extension recommends, so a client notifies a change and the server asks
for it. A selection the compositor announces is read off the loop into a pipe
and kept as the shared clipboard, and the client is sent a notify; the text
goes when the client requests it, as the shared clipboard is then. A selection
that is cleared or stops being text is kept as empty text, and notified as
holding nothing. A client's notify is answered with a request, and the text it
provides becomes the shared clipboard and a data source that takes the
selection; the compositor announcing that selection back is ignored while the
source is ours. A clipboard message that cannot be read is dropped, not the
connection.

## Security

RFB 3.8 with the configuration's types on offer: RSA-AES at both widths,
`RA2_256` first, when either `[pam]` or `[password]` is set, or None alone when
neither is. Classic VncAuth is deliberately absent: it proves knowledge of a
machine's secret, names nobody, truncates the password to eight characters, and
protects the login and nothing after it. With no login configured the session is
open and in the clear, so the listen address is a loopback or VPN address by
design.

RSA-AES is RealVNC's type as `rfbproto` documents it and TigerVNC, neatvnc and
the remotex gateway speak it: the server's RSA key and a fresh client key are
exchanged in the clear, each side seals a random to the other's key, the two
randoms derive one AES-EAX key per direction, and from there every byte in both
directions travels in frames of `u16 len || ciphertext || tag` under a counter
nonce. Inside the frames each side proves the keys it saw with a hash, the
server asks for the credentials the configured login wants — subtype 1, a
username and a password, for `[pam]`; subtype 2, a password alone, for
`[password]` — and RFB's SecurityResult, ClientInit and everything after
follow. The credentials have one shape on the wire either way, a length-prefixed
username then a length-prefixed password, and a client answering subtype 2 sends
the username empty. `crates/wlshare-rfb/src/rsa_aes.rs` has the exchange byte by
byte.

The server's key is long-lived — generated once into `rsa_key_file`, logged as
RealVNC's eight-byte fingerprint at startup — because it is the one thing a
client can pin; remotex logs the fingerprint it saw on every connection.

What checks the credentials is `auth.rs`, one of two things. `[pam]` sends them
to PAM (`pam.rs`): `pam_authenticate` and `pam_acct_mgmt` under the configured
service, nothing else. Before PAM is asked, the username must be the account the
process runs as: wlshare injects input into one user's desktop, and another
account's password must not open it. `[password]` verifies the password against
an Argon2 PHC string from the configuration, in the parameters that string
carries, and names no account at all — for a host whose desktop user has no
system password to spend on a VNC client, or no PAM stack to spend it on. The
hash is parsed at startup, so an unusable one is a startup error and not a
surprise at the first client; `wlshare hash-password` prints one. An empty
password is refused before the hash is consulted. Either check blocks — a PAM
stack may sleep, Argon2 is slow on purpose — so both run on a blocking thread. A
refusal is answered after a one-second delay with SecurityResult failed and the
bare reason "authentication failed"; the actual reason is logged.

## Deliberately absent

Tight, TightPNG, Hextile, RRE, CopyRect and every lossy encoding but the VP9
one: the gateway re-encodes every tile anyway, and ZRLE is the standard's best
lossless choice. VP9 at 4:2:0, a VP9 quality above the configured one however
much room the link has, and the VP9 encoding for any client that does not list
it.
8- and 16-bit pixel formats and colour maps. Moving the client's pointer: the
PointerPos pseudo-encoding would carry a warp the compositor made, and the
cursor session does report positions, but only when the output repaints.
Multiple outputs in one framebuffer — a client picks one of them instead. A
control socket. A microphone format beside the one the server names. A V4L2 camera device for the client's camera: a PipeWire node
needs no kernel module and no privilege, at the cost of applications that open
`/dev/video*` alone not seeing it. Camera formats beside I420, and scaling a
camera picture to a size an application asks for.
