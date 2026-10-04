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

- [screen-vp9](https://github.com/andrewtheguy/screen-vp9), a repository of
  its own that this workspace and the remotex gateway each pin by release tag,
  is the one place libvpx is spoken to: the encoder configuration a desktop is
  coded with, the planes in front of it at either chroma, the decoder, what a
  frame says about itself, and the quality walk. It has no platform dependency,
  and a change to how a desktop is coded is made there once for the daemon and
  the gateway, and reaches each as a pin bump. See
  [The VP9 encoding](#the-vp9-encoding).
- `crates/wlshare-rfb` decides every byte on the wire: handshake, message parsing
  and building, RSA-AES and its frames, the ZRLE encoder, the VP9 encoding's
  framing over `screen-vp9`, the cursor and clipboard encodings, and the
  density, outputs, audio, camera and microphone extensions and the scroll
  message. It has no platform
  dependency — the audio extension's FLAC encoder, libFLAC under `sound-flac`,
  and its Opus encoder, libopus under `sound-opus`, are prebuilt static
  archives — and its tests decode every encoder's output with an
  independent decoder, read every server message back with a client's parser,
  and run the RSA-AES exchange against a client written from the specification.
- `crates/wlshare` is the daemon. `compositor.rs` is the Wayland thread and its
  command handler; `capture.rs`, `cursor.rs`, `outputs.rs`, `input.rs` and
  `clipboard.rs` are the protocols it speaks; `audio.rs`, `camera.rs` and
  `microphone.rs` are PipeWire's side, and `decode.rs` is the camera's libavcodec decoder;
  `framebuffer.rs` is the shared pixels and damage;
  `session.rs` is one client; `auth.rs` checks an RSA-AES login and `pam.rs` is
  the system half of that check; `state.rs` tells whoever connects to the state socket
  whether the desktop is held, and `watch.rs` is `wlshare watch`, the process
  that follows it and runs a command for each; `shared.rs` is what crosses
  between them.

Its client is the remotex gateway, which every viewer reaches the desktop
through.

## One session at a time

The desktop belongs to one client. A connection that finishes the handshake
takes it, and the client that held it is disconnected with a message naming the
one that took over — the same trade Windows Remote Desktop makes, and the reason
the takeover happens *after* the handshake: an unauthenticated connection, or
one whose login is refused, never displaces the session in progress. RFB's ClientInit
shared flag is read as nothing of the kind; there is no configuration for it and no way to
watch alongside somebody else. The one connection that does not take the desktop
is the client's own second, which shows it another output
([A display beside](#a-display-beside)).

The compositor thread holds the client id of each of the two, so the rule is one
comparison: input, resize, density, output selection and clipboard from anyone
else are dropped, which is what
a superseded session's last in-flight messages are. Taking over releases the
keys and buttons the previous client held and its pending resize, and capture
runs from the first handshake until the client on the desktop leaves.

Who is where is a `watch` and not one of the broadcast events: a session slow
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

### The state socket

`state_socket` names a Unix socket on which the daemon says whether a client is
on the desktop, so the session can be rearranged around the client: sway can be
told to disable the monitors, which moves their workspaces onto the headless
output the client is shown, and to enable them again when the client is gone.
The daemon itself does nothing to the session and runs nothing: what to do is
the compositor's and the operator's, and so is noticing that the daemon has
gone, which a daemon cannot be relied on to say.

The headless output need not be there while nobody is watching. The configured
`output` is a preference: with no output of that name the daemon shares another
and moves the desktop to the configured one each time it appears — on its
appearing only, so a client that then names another output stays where it asked
to be. A client takes the desktop before its ServerInit rather than after, so
the socket says `held` while the client has still been told no size, and with
`output_wait_secs` set the session waits that long for the configured output to
be the shared one: whatever follows the socket enables it, and the size the
client is told first is that output's, not a monitor's it would have been shown
for a moment and whose mode it cannot change. The wait ends the moment the
output is shared and otherwise runs out, and the client starts on the output
shared meanwhile. Enabling it is not the daemon's: a compositor reports a
disabled head with no position, so a daemon enabling one through
wlr-output-management would be choosing where it goes.

A connection is told where the desktop stands — `held` or `free`, one word to a
line — and again each time that changes; it says nothing itself, and only the
account the daemon runs as can connect. The words follow the desktop, not the
connections: a takeover is the desktop passing from one client to another, held
before and after, and is not a change, and a display beside holds nothing. Each
connection reads the same `watch` of seats the sessions do, so what it is told
is where the desktop stands now, however many joins and leaves happened in
between.

The end of the stream is the third thing a follower is told, and the reason
this is a socket and not a command the daemon runs. The kernel closes it when
the daemon is gone, whether it stopped, panicked or was killed where it stood,
so a daemon that never got to say the desktop was free has said so all the same.
The file is removed by a daemon that stops in order and replaced by the next one
otherwise. A socket that still answers is another daemon's: a second one
refuses to start on it rather than take its followers, and a daemon removes
the file only while it is still the one it made.

### The watcher

`wlshare watch --held COMMAND --free COMMAND` is that follower: a process of its
own, started by the session — a user unit beside the daemon's, or the
compositor's `exec` — with the session's environment, which reads the same
configuration file for where the socket is. It runs *held* when a client is on
the desktop and *free* when nobody has been for `--free-after-secs`, through
`sh -c`. A daemon it cannot reach holds nothing, and the socket is tried again
every second, so the order the two start in does not matter and neither does
which of them is restarted.

It keeps the session at a state rather than reporting transitions. The first
command it runs is the one for where the desktop stands when it starts, since
it cannot know what an earlier watcher left behind, and it runs nothing until
that is known — the daemon has said, or cannot be reached — so a watcher
started under a client never frees the desktop first; after that a command runs
only when the desktop stands otherwise than the last command said. Both
commands therefore have to be safe to run on a session already as they would
leave it. The wait before *free* is what keeps a flapping link from flapping
the monitors, and a daemon restarted by its unit from doing the same: the
client that comes back inside it, through the same daemon or the next, runs
neither command.

One command runs at a time. When it exits, the command for where the desktop
stands *now* runs if that differs from what the last one said, so a slow *held*
followed by a leave is followed by *free*, and a slow *held* followed by a
leave and a return by nothing. The desktop is watched while a command runs, so
the wait counts from when the client left and not from when the command ended.
A command still running at `--timeout-secs` is killed rather than waited on,
because the one that puts the monitors back is the one an operator is counting
on; each runs as a process group of its own and the group is what is killed, so
what the shell started goes with it; the same happens to a command whose task
is dropped under it. A command's exit status is logged and changes nothing
else.

A watcher that stops while its last command said the desktop was held — on
SIGINT or SIGTERM — runs *free* before it exits, without the wait and after any
command still running. One that is killed where it stands leaves the session as
it was, and the watcher its unit starts in its place runs the command for where
the desktop stands, which is what puts it right.

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

A client gets a pixel update when it has asked (`FramebufferUpdateRequest`, or
once for all with continuous updates) and there is damage. Each pixel update is
one `FramebufferUpdate` of merged rectangles, at most 32, ZRLE-encoded on the
client's own deflate stream, or Raw before the client's first `SetEncodings`
and for a client whose list names neither ZRLE nor VP9 — or, for a client that
lists the VP9 encoding, one rectangle of the whole framebuffer
([below](#the-vp9-encoding)).

With Fence negotiated, every pixel update ends with a fence the client echoes,
and the next pixel round waits for the echo. Cursor, audio and geometry
announcements sent with a request precede any pixels for it and have no fences
of their own. One pixel round is in flight at a time, so a slow link is never
flooded and frames coalesce in the framebuffer meanwhile. remotex negotiates
both ContinuousUpdates and Fence.

What the echo means is the client's to decide, and remotex holds it while the
picture is the VP9 stream until the browser has taken the frame, rather than
answering the moment it arrives ([below](#the-clients-paint)).

A size change goes out first, as its own update — an ExtendedDesktopSize
rectangle whose reason says who asked (the server, this client, another
client), or a DesktopSize rectangle for a client without the extension — and the
whole framebuffer follows in the next update. A client that negotiated neither
cannot be told and is disconnected at its next update rather than sent pixels at
a size it does not know. The ExtendedDesktopSize announcement that answers the pseudo-encoding is an
update too, and waits for a request like any other.

## The VP9 encoding

A private encoding, `WLSV` (`0x574c5356`), for the remotex gateway: the whole
desktop as one VP9 stream, for a client that
would rather have a picture that moves than one that is exact. While the
framebuffer is within its video ceiling, remotex lists it for every browser on a
target with `subtype = "wlshare"` and passes each frame to the browser as it came, since it is the stream remotex would
otherwise encode from ZRLE's pixels — at the chroma the browser's decoder takes
and the target's own dial, which it names beside the encoding, below. A client
that does not list it is unchanged.

A client that lists it, with a quality beside it (below), gets it instead of a
standard pixel encoding, wherever in the list it is; listed without one it is
fatal. Each pixel update is then one rectangle covering the whole
framebuffer, whose body is a length word and one VP9 frame:

```text
u32 length   the frame's bytes
u8[length]   one VP9 frame
```

Successive rectangles are one stream, each frame coded against the ones before
it, so a client decodes them all with one decoder, in order. The coding is
screen-vp9's, which remotex encodes its own streams with as well, so
the two sides agree on every libvpx setting by construction: the quantizer
pinned to the dial, screen-content tuning, no lag, no dropped frames, no
keyframe that was not asked for, and the colour declared in the bitstream.
`crates/wlshare-rfb/src/vp9.rs` is the framing over it — the length word and
its ceiling, the framebuffer's pixels in and out. What a frame holds is fixed:

- **8-bit 4:4:4, VP9 profile 1, unless the client asks for 4:2:0.** A colour
  sample per pixel: the loss 4:2:0 costs a desktop is its text's colour — a
  one-pixel coloured stem shares its sample with three pixels of background —
  and no quantizer puts it back. The gateway asks for 4:2:0 (profile 0) for a
  browser whose decoder takes nothing else, since a stream that browser refuses
  by name carries no colour at all, by listing `WLS0` beside the encoding
  (below).
- **BT.601 at studio swing**, converted from the framebuffer's `B, G, R, X` and
  declared in the keyframe header, so a decoder converts back with the same
  matrix. The client's pixel format does not apply.
- **A quantizer and a frame rate that follow the link.** The 1–100 dial maps
  onto VP9's 8–63, finest last, as remotex's dial does; rate control is pinned
  to wherever the dial is, with no bitrate, no adaptive quantization and no
  dropped frames. A session starts at the ceiling the client's list names
  (`WLQ`, below), which it never goes above, and
  walks down to a floor of 20 while the client is behind — the one walk both
  run, screen-vp9's `walk`. The floor is a constant, not a key, as
  every adaptive stream's is: where WebRTC's quality scaler hands off from the
  quantizer to resolution and frame rate at its own threshold, this walk hands
  off to the frame rate, and the settle below sharpens a quiet desktop back at
  the ceiling.
  A frame's queueing is its fence's round trip — answered once the client has
  the frame, which for remotex means once the browser has taken it
  ([below](#the-clients-paint)) — less the shortest of the last
  minute's, so distance does not read as queueing; a keyframe counts towards
  that floor but is no verdict. Without Fence — and for a held dial, fence or
  no fence — it is how long writing the frame blocked, 20 ms of it being behind. Two
  behind frames (60 ms of queueing or more) among the last four give quality
  up: ten points, twenty at 150 ms, thirty at 400 ms; once the dial is on the
  floor the capture's frame interval doubles instead, 33 ms up to 133 at the
  default `max_fps`, and at 400 ms both go at once. The interval is read when
  the next frame is due, so a step the last frame's fence brings paces that
  frame. The cursor, the audio announcement and a desktop that changed size
  never wait for a slowed frame's turn. A step is taken at most once a second, and while the lag
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
- **What the client lists beside it says what the stream is to be**, the way
  Tight's quality levels ride `SetEncodings` — pseudo-encodings rather than a
  message, because a server that is not wlshare ignores an encoding it does
  not know where a message it does not know ends the connection, and because
  they ride the list that names the encoding, so the first frame is already
  what was asked for. `WLS0` (`0x574c5330`) asks for 4:2:0 in place of 4:4:4;
  `0x574c5100` plus a quality 1–100 (`WLQ` and the value) names the ceiling the
  walk never goes above; `WLSD` (`0x574c5344`)
  holds the dial there, with a walk that hears nothing in a fence — only a
  frame whose write blocked moves it, fence or no fence — and a settle with
  nothing to sharpen. The gateway lists them from the target's keys, so
  `render_chroma`, `video_quality` and `render_adaptive` mean on a passed
  stream what they mean on one the gateway codes. Read at every
  `SetEncodings`, and owed a frame whether or not the desktop changed: a new
  chroma starts the stream over at a keyframe, a new ceiling moves the running
  encoder's dial and sends the picture once at it, as a settle does, and a new
  walk moves the dial alone. A list that names VP9 without a quality is
  fatal, as a malformed message is: wlshare has no quality of its own, the
  gateway names one beside the encoding on every list, and a client that is
  not the gateway — a plain `vnc` target reads wlshare through the RFB
  baseline — lists no VP9 at all. Without `WLS0` the stream is 4:4:4, and
  without `WLSD` it walks.
  Screen-content tuning, libvpx's
  realtime speed 7, no lag, and threads with row and tile parallelism: the
  machine's cores less two, at most eight, for the encoder, since an encode
  is a burst the person at the other end waits on, and libvpx clamps the tile
  columns to what the width allows; half the machine, at most four, for the
  decoder, which gains nothing past the stream's tiles. The conversion
  between the framebuffer's pixels and the planes is the `yuv` crate's, on
  the AVX2 or NEON path the machine has: a scalar loop over a 4K frame was a
  fifth of an encode and a third of a decode.
- **A quiet desktop is sharpened at the ceiling, and the walk keeps its
  place.** The walk only runs when a frame goes out. Ordinarily a frame only
  goes out when something changed, so a desktop that stops right after the link
  coarsened it would keep that picture until it changed again. Once a frame
  encoded below the ceiling has been delivered — its fence answered, or
  without Fence its write finished — and nothing has been sent for 500 ms
  since, the unchanged picture goes out again, at the next update the client
  asks for, as one inter frame at the ceiling: libvpx codes the residual of
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
  frame that went out at the ceiling owes nothing, and a desktop that goes
  quiet after one sends nothing. remotex's settle for its own whole-desktop
  streams.
- **Keyframes only when a decoder needs one**: the first frame after the
  encoding is listed, the first at a new size or chroma (the encoder is made
  again for it), and the frame that answers a non-incremental request. There is
  no periodic keyframe; nothing is lost on TCP.

A normal VP9 update is sent when anything is damaged; a non-incremental request,
a chroma or quality-ceiling change and the settle described above can also owe
one. The frame is the whole picture: the encoder's inter-frame coding is what
makes an unchanged region cost nothing. The encode runs on the session's worker,
which is told it is blocking, and the fence keeps one frame in flight as it does
a standard pixel update. A `SetEncodings` that drops the encoding is answered
with the whole framebuffer in the standard encoding it selected — ZRLE when
listed, Raw otherwise — since the client is holding a lossy picture.

libvpx comes from `libvpx-prebuilt`'s static archive, through screen-vp9.

### The client's paint

The walk above is only as honest as the fence it times, and a fence echoed the
moment a frame arrives times the link to the gateway and nothing else. A
browser that cannot take frames as fast as the desktop draws them would read as
a healthy client: the server would keep coding at the dial, and the person
would watch a picture that lags while every measurement said the link was
clear.

So remotex holds each echo for what the browser's link makes of the frames
before it, measured by the `paintAck` the browser sends after its painter
draws. The path a frame takes to the screen is then inside the round trip the
walk reads, and, because the server sends nothing until the echo arrives, a
frame the browser would only have overwritten unseen is never encoded at all.
A browser that is not drawing must not be able to stop the desktop, so no echo
is held past 500 ms — remotex's `FENCE_HOLD_LIMIT`, its paint window's grace.

## The density extension

Standard RFB has no word for pixel density. The extension is one pseudo-encoding,
`0x574c5348` (`WLSH`), and one message type, `0xE0`, in both directions; scales
are 16.16 unsigned fixed point, so `0x0002_0000` is 2.0 and `0x0001_8000` is 1.5.
Both messages have one layout:

| Offset | Type | Field |
|---|---|---|
| 0 | U8 | message type, `0xE0` |
| 1 | U8 | padding |
| 2 | U16 | width, pixels |
| 4 | U16 | height, pixels |
| 6 | U32 | scale, 16.16 fixed |

- **OutputScale**, server → client, ten bytes: type, padding, width and height
  in pixels, scale. Sent as the answer to *every* `SetEncodings` that lists the
  pseudo-encoding — the only way support is announced — and whenever the shared
  output's scale or mode changes or another output becomes the shared one, before
  the frame at the new size is captured, so the report precedes the resize
  rectangle.
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
  The id is the `wl_output` global, unique for as long as the output exists and
  opaque to the client. The name is the compositor's own, `DP-2` or
  `HEADLESS-1`, and the headless flag marks an output the compositor made rather
  than a monitor somebody is sitting at.

| Offset | Type | Field |
|---|---|---|
| 0 | U8 | message type, `0xE1` |
| 1 | U8 | padding |
| 2 | U16 | count |
| 4 | U32 | the shared output's id |

  then `count` entries:

| Offset | Type | Field |
|---|---|---|
| 0 | U32 | id |
| 4 | U16 | width, pixels |
| 6 | U16 | height, pixels |
| 8 | U32 | scale, 16.16 fixed |
| 12 | U8 | flags — bit 0: headless |
| 13 | U8 | name length |
| 14 | U8[] | name, UTF-8 |

- **SelectOutput**, client → server, eight bytes: type, three bytes of padding,
  the id of the output to share. Honoured only from a client that listed the
  pseudo-encoding and holds the desktop, and **answered with an OutputList**
  either way: a request naming an output the compositor no longer has, or the one
  already shared, is answered with the list as it is. So a client's menu follows
  what is on the canvas rather than what was clicked.

| Offset | Type | Field |
|---|---|---|
| 0 | U8 | message type, `0xE1` |
| 1 | U8[3] | padding |
| 4 | U32 | id |

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

### A display beside

One framebuffer is one output and one connection is one framebuffer, so a client
that wants two outputs at once connects twice. Its second connection finishes
the same handshake, login included, and sends `0xB5` as its ClientInit byte
where RFB has a shared flag. It then does not take the desktop: the client on
it stays, and this connection is shown the first output the list has that the
client is not on. Every other ClientInit value takes the desktop, RFB's `1`
included.

It is a whole RFB session over that output: its own framebuffer and capture,
its own cursor session, a virtual pointer made against its output, its own
encoding — VP9 with a walk of its own link, where it lists it — and its own
resize and density, under the rules of the output it is on. Its ServerInit
names that output's size, so it is sent only once the output is its own. The
keyboard is the seat's, and keys from either connection reach whatever the
compositor has focused; what each holds is let go when it leaves, and nothing
the other holds. A key both hold goes up when the last of them lets go of it.
The clipboard is the desktop's and is set by the client on
it alone.

What it may not do is choose. Which output is where is the client's on the
desktop: the connection beside is sent an `OutputList` naming its own output
as the shared one if it lists the extension, and its `SelectOutput` is answered
with that list as it is. It ends, its socket closed, when

- the client on the desktop leaves or is taken over — a display beside is that
  client's, and a new client starts with none;
- the client on the desktop selects the output it shows, since an output is on
  one of them;
- another connection asks to be beside, which takes its place;
- the compositor takes its output away.

One that asks with nobody on the desktop, or with no output the client is not
on, is closed before ServerInit. So is one whose handshake finished after a
later connection's display beside had already ended: it takes nobody's place.
And so is one that connected before the client now on the desktop did: it was
opened beside whoever was there before.
There is one beside, so two outputs at once is
the most a client is shown. The remotex gateway opens it for the second
display's browser tab on *All Displays*, and lists on it the pixel encodings,
the cursor, the size and the density and nothing else.

Only a headless output is ever resized or rescaled, so switching to a real
monitor leaves a client's resize and density requests answered *prohibited* —
that monitor's mode belongs to the person sitting at it.

The choice outlives the client that made it: the next connection opens on the
output the last one asked for, not on the configured default, until the daemon
restarts. Measured on a two-monitor sway session in
[remotex's `docs/wlshare-outputs.md`](https://github.com/andrewtheguy/remotex/blob/main/docs/wlshare-outputs.md),
which is where the gateway's half of this lives.

## The audio extension

The desktop's sound, as FLAC or as Opus, on the connection the pixels use. It
is private — pseudo-encoding `0x574c5346` (`WLSF`) and server message type
`0xE4` — and its client is the remotex gateway. Which codec is the client's to
ask for, as the format is, and nothing here configures it: FLAC, unless the
client lists `0x574c4f50` (`WLOP`) beside the encoding. The client's messages and the stream's
begin and end are borrowed from the QEMU Audio extension `rfbproto` registers,
message type `255` submessage `1`; QEMU's pseudo-encoding, `-259`, is not
spoken, because what it promises is raw samples and none are sent. A client that
lists only `-259`, gtk-vnc for one, hears nothing.

- **The announcement**, server → client: an empty pseudo-rectangle of encoding
  `WLSF` in a `FramebufferUpdate` of its own, sent ahead of any pixels to a
  client whose `SetEncodings` listed it. The only way support is announced.

| Offset | Type | Field |
|---|---|---|
| 0 | U16 | x, 0 |
| 2 | U16 | y, 0 |
| 4 | U16 | width, 0 |
| 6 | U16 | height, 0 |
| 8 | S32 | encoding, `0x574c5346` |

- **Set format, enable, disable**, client → server, QEMU's messages: the sample
  format, channel count and frequency are the client's to choose, and the
  server converts what the desktop plays into them. The formats are QEMU's
  codes 0–3, U8, S8, U16 and S16; its 32-bit codes are refused, because FLAC
  stores at most 24 bits. The frequency is bounded at 8 kHz, the lowest rate
  real audio uses, and at 96 kHz, twice what the desktop's own graph runs at.
  A code or rate outside those is fatal. Four bytes for an enable or a disable,
  ten for a set-format:

| Offset | Type | Field |
|---|---|---|
| 0 | U8 | `255` |
| 1 | U8 | `1` |
| 2 | U16 | operation: 0 enable, 1 disable, 2 set format |
| 4 | U8 | sample format (set format only): 0 U8, 1 S8, 2 U16, 3 S16 |
| 5 | U8 | channels, 1 or 2 |
| 6 | U32 | frequency |

- **Opus**, client → server: the pseudo-encoding `WLOP`, listed beside `WLSF`,
  asks for the sound as Opus in place of FLAC. A pseudo-encoding rather than a
  message, as the VP9 stream's choices are, so it rides the list that asks for
  the sound at all. A list that changes the codec while a stream runs restarts
  the stream in the new one, between an end and a begin.
- **Set bitrate**, client → server, operation `3` beside QEMU's three and
  wlshare's own: the rate Opus is coded at, in bits per second, 6 000 to
  510 000, libopus's bounds; a rate outside them is fatal. An Opus stream has
  no rate of its own: one must have been set before its enable, and an enable
  without one is refused with a log line and no begin, as one at a frequency
  Opus does not code is. It may come again while the stream runs, and the
  running stream moves to it at its next packet with no restart. A FLAC stream
  has no rate to move.

| Offset | Type | Field |
|---|---|---|
| 0 | U8 | `255` |
| 1 | U8 | `1` |
| 2 | U16 | operation, `3` |
| 4 | U32 | bits per second |

- **Begin and end**, server → client, QEMU's messages:

| Offset | Type | Field |
|---|---|---|
| 0 | U8 | `255` |
| 1 | U8 | `1` |
| 2 | U16 | operation: 0 end, 1 begin |

- **A frame**, server → client, between a begin and an end — one FLAC frame, or
  one Opus packet:

| Offset | Type | Field |
|---|---|---|
| 0 | U8 | message type, `0xE4` |
| 1 | U8[3] | padding |
| 4 | U32 | length of the frame |
| 8 | U8[] | one FLAC frame or one Opus packet |

Every FLAC frame holds exactly `frequency / 50` frames of samples, rounded down —
twenty milliseconds, 960 at 48 kHz — in fixed-blocking mode, and is numbered
zero. The FLAC stream header, `STREAMINFO`, is never sent: everything
in it is already agreed, so a client builds it from the format it set, with
that block size as both minimum and maximum. A frame states its own rate unless
no frame header has a code for it, as for 70001 Hz, and then leaves it to that
header. An unsigned format has the top
bit of every sample flipped before it is encoded, which maps its range onto the
signed one of the same width with silence on zero; the client flips it back.
Decoded samples are interleaved and little-endian, and bit for bit what the
capture produced.

FLAC is lossless, while music and speech cost about two-thirds of their 1.5 Mbit/s PCM rate or less and
a silent desktop a few bytes a frame. A frame length past 64 KiB is fatal to a
client: the largest block there is, 20 ms of 16-bit stereo at 96 kHz, is 7680
bytes before compression.

The encoder is libFLAC, the reference one, spoken to in one place:
[sound-flac](https://github.com/andrewtheguy/sound-flac), a repository of
its own that this workspace and the remotex gateway, whose decoder it also is,
each pin by release tag. It links FLAC 1.5 from a prebuilt static archive
([libflac-prebuilt](https://github.com/andrewtheguy/libflac-prebuilt)), so no C
is compiled, nothing is loaded at run time and the package depends on no
libFLAC. libFLAC encodes a stream and holds each block back until it has a sample
of the next, to know whether the block is the stream's last; sound heard as it
is made cannot wait twenty milliseconds for that, so each frame is a stream of
its own, one block long, started and finished around it, which is why every
frame is numbered zero. The marker and metadata such a stream opens with are
dropped. `wlshare-rfb`'s tests read every frame back with symphonia's decoder,
which shares nothing with libFLAC.

As Opus, every packet is twenty milliseconds too, `frequency / 50` frames, and
the frequency must be one Opus codes at — 8, 12, 16, 24 or 48 kHz — or the
enable is refused with a log line and no begin. The sample format says only
what the capture hands the encoder, an unsigned sample flipped as for FLAC and
an 8-bit one widened to sixteen: what a decoder gives back is its own business.
No `OpusHead` is sent. A client builds it from the format it set, with a
pre-skip of 312, the encoder's lookahead in 48 kHz samples at every frequency.
A packet decodes from the ones before it and the wire numbers none, so a client
could not tell one was missing: wlshare drops no packet it coded, and a client
that fell behind loses sound that was never coded instead.

It is the choice of a client whose own listener takes Opus: the remotex gateway
hands each packet to the browser as it came, where it would otherwise decode
the FLAC and code Opus itself, and sends the rate the walk of the browser's
link arrives at as a set-bitrate: sound-opus's `walk`, the one rate walk
there is, which the gateway runs for the sound it codes itself and for this. The encoder is libopus, spoken to in one
place as libFLAC is: [sound-opus](https://github.com/andrewtheguy/sound-opus),
which this workspace and the gateway, for the sound it codes itself, each pin
by release tag, so a packet made here and one made there are the same stream —
music-tuned, constrained variable rate around the bitrate, at full effort, with
silence a few bytes a packet. libopus is linked from a prebuilt static archive
its sys crate downloads, as libvpx is under `screen-vp9`: no C is compiled,
nothing is loaded at run time and the package depends on nothing for it.
`wlshare-rfb`'s tests wrap the packets in an Ogg stream behind the header a
client builds and decode them with FFmpeg's own Opus decoder, which shares
nothing with libopus, so they need `ffmpeg` on the path.

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
requested one, so the first buffers of a session are often shorter than 20 ms,
512 frames where 960 were asked for —
and queues each frame it completes in a sixteen-deep queue. When a client cannot
keep up, FLAC drops the oldest: each frame decodes on its own, so a dropped one
is a 20 ms hole, and a stalled capture callback is worse. Opus leaves a buffer
uncoded while the queue is full instead, so the decoder is handed every packet
the encoder made and the two stay in step across the hole. A set-format on a
running stream, or a list that changes its codec, restarts the capture as now
asked for, holding the speaker
across so the host is not heard between the two captures, and a disable or a
disconnect stops it; what is left of a frame goes with it.

The session drains that queue before every framebuffer update, so sound is never
held behind a pixel update it was ready before. A headless session needs no sink
of its own: the speaker is one.

The announcement is off unless the configuration sets `audio = true`; while it
is off, a client that lists the pseudo-encoding is told nothing.

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

The announcement is off unless the configuration sets `camera = true`; while it
is off, a client that lists the pseudo-encoding is told nothing.

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

The announcement is off unless the configuration sets `microphone = true`;
while it is off, a client that lists the pseudo-encoding is told nothing.

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
tracked with `xkb_state` and sent after every key. The state is fed a keycode's
transitions only: xkb counts a modifier key's presses and holds the modifier
until as many releases arrive, so a repeat of a held modifier — the browser
auto-repeats Control like any key on some platforms — is forwarded as a key
and kept out of the state. A character keysym names a
character the client has already cased — remotex never forwards Caps Lock and
sends `A` or `a` as the browser resolved it — so before each press the server
checks what the keycode would produce under the current modifiers, and presses
Shift or lets a held Shift go around the key when the keycode alone would type
the other case. A Shift pressed for a key goes up with the key unless the client
has since pressed that Shift key itself, which makes it the client's to let go
of. A keysym that names a key rather than a printable character is
exempt and goes out on its keycode under whatever the client holds: Shift+Tab
arrives as Shift then `Tab`, the keycode's shifted level is `ISO_Left_Tab`, and
letting Shift go to make it produce `Tab` would type a plain Tab.
Keys and buttons are let go when the client leaves or is superseded, and a
connection that never finished the handshake releases nothing. Pointer events
arrive in framebuffer pixels and are injected as absolute positions against the
framebuffer's extent, which the virtual pointer maps onto the shared output — a
pointer per connection, each on its own output, moving the seat's one cursor.
Wheel "buttons" become discrete axis events, a notch apiece.

A notch is all RFB can say, so a touchpad glide or two fingers on a phone would
scroll in lurches. A client that knows the server is wlshare sends a distance
instead: the private message `0xE5`, six bytes — the type, a byte of padding,
and the horizontal and vertical distance as S16 logical pixels of the output,
positive rightward and downward — after the `PointerEvent` that says where. It
has no pseudo-encoding, the server having no state to hold for it and nothing
to answer. Logical pixels are the units an axis is in, so the distance is
injected as it arrives, as a continuous axis: what a touchpad plugged into the
machine sends, which an application spends as the distance it is.

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
lossless choice. A VP9 quality above the ceiling the stream was asked for
however much room the link has, the VP9 encoding for any client that does not
list it.
8- and 16-bit pixel formats and colour maps. Moving the client's pointer: the
PointerPos pseudo-encoding would carry a warp the compositor made, and the
cursor session does report positions, but only when the output repaints.
Multiple outputs in one framebuffer — a client picks one of them, or connects
again for another beside it. More than two outputs at once. A
control socket. A microphone format beside the one the server names. A V4L2 camera device for the client's camera: a PipeWire node
needs no kernel module and no privilege, at the cost of applications that open
`/dev/video*` alone not seeing it. Camera formats beside I420, and scaling a
camera picture to a size an application asks for.
