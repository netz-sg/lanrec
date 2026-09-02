# lanrec

Record a Windows gaming PC over a direct Ethernet cable to a second machine, at
a quality where the compression is not the limiting factor.

*[Deutsche Fassung](README.de.md)*

---

## What it does

You are playing on one machine and want a clean recording without that machine
doing the recording. lanrec captures the screen, encodes it on the GPU's video
engine, and streams it over a dedicated Ethernet link to a second computer that
writes it to disk.

Nothing on the hot path touches system memory. The captured frame arrives as a
GPU texture, is handed to NVENC as a GPU texture, and leaves as a compressed
bitstream. The receiver never decodes anything — it appends framed payloads to a
file, so it stays close to idle even at a few hundred megabits per second.

**Sender** needs Windows, Direct3D 11 and an NVIDIA GPU.
**Receiver** depends on nothing but the Rust standard library. It builds on
macOS, Linux and Windows.

### Why not just use a capture card?

If you want *literally zero* load on the gaming PC, buy an HDMI capture card —
that is the honest answer, and this tool will not beat it. lanrec exists for the
case where you would rather spend nothing and accept ~0–3 % GPU utilisation for
the encoder block, which is otherwise sitting idle anyway.

### Why not just use OBS?

OBS with an SRT output does roughly this, and if that already works for you, use
it. lanrec is narrower on purpose: one link, one profile, no scene graph, and
every design decision tuned for a direct cable rather than the general case. It
also tells you *why* a setting is bad for your hardware instead of letting you
find out at encoder init.

## How it works

```
Gaming PC (Windows)                          Receiver (anywhere)
───────────────────                          ───────────────────
Windows.Graphics.Capture
  → ID3D11Texture2D                stays in GPU memory
        │
      pace                         fixed-rate grid, reports empty slots
        │
      NVENC                        texture in, bitstream out — zero copy
        │
      wire                         [magic│ver│kind│pts│dts│flags│len][payload]
        │
       TCP  ──────────────────────────────────→  TCP
                                                   │
                                                 file            no re-encode
```

## Quick start

On the receiver:

```sh
cargo run --release -p lanrec-recv -- --listen 10.0.0.2:9000 --out-dir ~/recordings
```

On the gaming PC:

```sh
cargo run --release -p lanrec-cli -- send --to 10.0.0.2:9000 --via "Zum Mac" --seconds 600
```

`--via` names the adapter to send over -- by the name you gave it, its IPv4 or
its MAC. Without it the routing table decides, which with two adapters can
silently mean the wrong one. See [Forcing one adapter](#forcing-one-adapter).

Record locally, no network involved:

```sh
lanrec record --seconds 30 --qp 14 -o out.hevc
```

Find out what your machine can actually do:

```sh
lanrec caps        # what the encoder really supports, queried from the driver
lanrec nics        # adapters, link state, negotiated rate, MTU
lanrec monitors    # capturable displays
lanrec capture     # measure capture and pacing without encoding anything
```

There is also a desktop app (Tauri + React) with a live preview, the adapter
list with link status, and quality settings with a live bitrate estimate:

```sh
cd app && npm install && npm run tauri dev
```

## The bandwidth reality

This is the constraint everything else follows from.

| Signal | Bitrate |
|---|---|
| 1440p60 raw, 4:4:4 10-bit | 6.6 Gbit/s |
| 1440p60 raw, 4:2:0 8-bit | 2.7 Gbit/s |
| **1440p60 HEVC 4:4:4 10-bit, CQP 14–16** | **150–350 Mbit/s** |
| Usable on a gigabit link | ~940 Mbit/s |

Truly lossless is impossible over gigabit. What *is* possible is a quality level
where the difference from lossless is not findable in a still-frame comparison,
using about a third of the link.

## Design decisions

Each of these is a decision that could reasonably have gone the other way.

### HEVC 4:4:4, not AV1

AV1 is more efficient per bit. It is also, on Ada, limited to 4:2:0 — verified by
querying the driver rather than trusting the spec sheet:

```
             4:4:4   10-bit   lossless   max
HEVC          yes     yes       yes      8192x8192
AV1         → no      yes       no       8192x8192
H.264         yes     no        yes      4096x4096
```

Efficiency is not the binding constraint here; there are ~600 Mbit/s of headroom.
What you actually see is chroma subsampling destroying HUD text, thin coloured
lines and saturated flat areas. You do not see AV1's advantage at this bitrate.

lanrec queries these capabilities at startup and only offers settings that will
survive encoder initialisation. Switch the codec to AV1 in the UI and 4:4:4
greys out, because on this hardware it genuinely cannot work.

### 10-bit even for SDR content

The source is 8-bit BGRA — the desktop compositor does not produce anything
else for SDR. The extra precision is used by the *encoder*, and encoding is
where banding in gradients (sky, smoke, fog) gets introduced. It costs almost
nothing on Ada.

### CQP, not CBR

Constant quality rather than constant bitrate. Quiet scenes use less of the
link, explosions are allowed to spike. There is no bitrate target to hit because
the link is not the bottleneck.

### TCP, not SRT

SRT was the original plan, and over a switch or a WAN it remains the right
answer. On a direct cable between two machines there is no competing traffic, no
congestion and effectively no loss — SRT's retransmission machinery buys nothing
there while costing a C dependency on both platforms. A *recording* also does
not care about the latency spike of a TCP retransmit; it cares that every byte
arrives.

The framing is transport-neutral. Swapping in SRT touches only the socket.

Both ends raise their socket buffer to 8 MB. The OS default stalls a
few-hundred-megabit stream: the window closes, the sender blocks, and the
capture queue behind it starts dropping frames.

### Frames are repeated into empty slots

This one came out of a measurement that contradicted the design.

Windows.Graphics.Capture is driven by the compositor, not by vblank. Where
nothing changes, nothing is delivered. Measured on the target machine, an idle
165 Hz desktop produces about **48 frames per second**:

```
$ lanrec capture --seconds 6 --fps 60

Input       289 frames  (48.2 fps)
Output      360 frames  (60.0 fps)
  new       289
  repeated   71
```

A constant-rate stream therefore has to fill those slots itself. Without it, a
recording of a paused game becomes a hole in the timeline and everything after
it drifts — the kind of error you discover during editing.

The pacer reports empty slots as `gap_slots` and leaves the policy to the
caller: a recording repeats the previous frame (identical content costs almost
no bitrate), a live view might rather skip.

Reporting on the *next* frame is not enough on its own, though. If the screen
stops changing completely, no next frame ever arrives -- a four-second recording
of a frozen desktop produced two frames, because the trailing gap was never
closed and the receiver had nothing to write. So the timeline is also advanced
from the clock (`Pacer::catch_up`) whenever waiting for a frame times out, and
once more at the end of a run.

### 165 → 60 will always judder a little

165 / 60 = 2.75. Not an integer ratio, so runs of 2 and 3 frames have to be
discarded alternately no matter what. That judder cannot be removed, only
distributed.

Taking "the first frame at or after each tick" is causal and free, but the
selection error swings across a full input interval (6.1 ms at 165 Hz) in a
pattern that beats against the output rate — visible as irregular stutter.
lanrec holds one frame back instead, so each tick picks whichever of its two
neighbours is closer. That halves the worst-case error to ±3 ms and makes it
uniform, at the cost of exactly one input frame of latency.

**The clean fix is on your side:** cap the game at 120 fps and record at 60. A
2:1 ratio has no ambiguity at all.

### Forcing one adapter

With two NICs, `connect()` alone does not choose: the routing table does. Where a
direct cable and a router link both exist, a stream meant for the cable can end
up going through the house network, competing with everything else and never
reaching the rate the cable could carry. Nothing in the socket API defaults to
"the one I meant".

`--via` therefore does three things, because no single one of them is airtight:

1. **`IP_UNICAST_IF`** pins the outgoing interface explicitly, outranking the
   routing table. (socket2 exposes this on Unix only, so it is set directly.)
2. **Binding the socket to that adapter's address** before connecting. Windows
   uses the strong host model for sending, so a socket with that source address
   leaves through that adapter.
3. **Reading `local_addr()` back after connecting** and failing loudly if it is
   not the address that was asked for.

The third step is the one that matters. A socket option that silently did nothing
would otherwise look exactly like success.

Resolution is strict on purpose: a spec matching two adapters is an error rather
than a guess, and an adapter with no link or no IPv4 is refused before the
encoder is even started.

The receiver side is already explicit -- `--listen 10.0.0.2:9000` binds to one
address. Use the direct link's address rather than `0.0.0.0` if you want it to
accept from that cable only.

### The preview does not read back frames

Reading a full 1440p frame to show a 320-pixel-wide preview would push ~850 MB/s
across the bus — precisely what this pipeline exists to avoid. Instead the frame
goes into mip 0 of a texture with a mip chain, `GenerateMips` fills the rest, and
only mip 3 (320×180) is read back and JPEG-encoded for the UI.

It runs on its own thread with its own D3D11 device, so the preview can never
contend with a running recording for the same context.

## Repository layout

```
crates/lanrec-wire/    Wire format. No platform dependencies.
crates/lanrec-core/    Capture, encode, pacing, preview, adapters. Windows.
crates/lanrec-cli/     Sender, headless.
crates/lanrec-recv/    Receiver. Builds anywhere.
app/                   Tauri 2 + React desktop app.
vendor/                nv-codec-headers (NVENC API, MIT) — see NOTICE.md
```

The core deliberately depends on no UI. When a recording drops frames, the first
question is always whether the app or the pipeline is at fault — so the pipeline
has to be startable and measurable without a window.

### The wire format

Both ends are ours, so the stream needs no container. MPEG-TS would force PCR
handling, 188-byte padding and a 90 kHz timestamp grid. A fixed 32-byte header
keeps nanosecond timestamps and leaves room for metadata a container has no
place for:

```
magic  u32   "LANR"
version u16
kind   u16   info | video | audio | end
pts    u64   nanoseconds
dts    u64
flags  u32   bit 0 = keyframe
len    u32
```

A JSON `StreamInfo` message precedes the media, so the receiver can report the
geometry and name the file without parsing the bitstream.

### NVENC bindings

Generated at build time from vendored `nv-codec-headers` (MIT, no NVIDIA account
required). Two things that are easy to get wrong and were worth automating:

- The header's codec GUIDs are `static const GUID`. bindgen turns those into
  extern statics with no symbol to link against — it fails at link time, far from
  the cause. They are extracted from the header text as real constants instead.
- The `*_VER` struct-version constants come from a function-like macro that
  bindgen silently skips. Without them the driver rejects every call with
  `NV_ENC_ERR_INVALID_VERSION` and no hint as to which struct was at fault. They
  are re-derived from the header rather than transcribed, because they change
  between SDK releases.

API 13.1 also replaced `pixelBitDepthMinus8` with `inputBitDepth`/`outputBitDepth`
enums — which is exactly why the bindings are generated and not written by hand.

## Two traps worth knowing about

**One immediate context, two threads.** Capture runs on a thread-pool thread, the
encoder on the caller's. Both need the same `ID3D11DeviceContext`, which is
explicitly not safe for concurrent use. Handing out clones instead of a shared
lock wedges the driver — no error, no crash, the process simply stops making
progress. `Gpu` therefore only ever hands out the lock.

**`nvEncLockBitstream` blocks on an empty buffer.** In synchronous mode without
B-frames the bitstream is locked and emptied after every encode, so nothing is
ever pending. An extra lock during the EOS flush does not report "empty" — it
waits forever for output that will never be produced.

## Known limitations

1. **Colour conversion is the driver's.** NVENC receives BGRA and does RGB→YUV
   itself. Which matrix and which range it uses is not under our control. For a
   tool aiming at maximum quality this is the next thing to fix; a compute shader
   doing the conversion explicitly solves it.
2. **No audio yet.** And with it, the hardest part of the whole project: the
   sound card does not run at exactly 48 kHz and its clock has nothing to do with
   QPC. Without drift correction the audio is 200–400 ms out after ~40 minutes.
3. **The WGC session breaks** on alt-tab, resolution changes and fullscreen
   toggles. It needs to be rebuilt without interrupting the recording.
4. **The bitrate estimate is a model, not a measurement.** Log-linear, with fixed
   factors for chroma, depth and codec. It reliably tells 150 Mbit/s from 400,
   and not 230 from 250. Calibrating it needs numbers from real gameplay.

## Status

- [x] Capture → NVENC → local `.hevc`, bitstream verified at the NAL level
- [x] Framing + TCP, receiver writes the file
- [x] Live preview, adapter naming, quality settings
- [ ] WASAPI loopback audio, A/V sync, drift correction
- [ ] Control channel, discovery, reconnect
- [ ] Explicit colour conversion; calibrate the bitrate model

## Building

Prerequisites are listed in [`docs/setup.md`](docs/setup.md): Rust (MSVC), VS
Build Tools, the Windows SDK, LLVM for bindgen, and Node for the app.

```sh
cargo build --release      # core, sender, receiver
cargo test --workspace

cd app && npm install
npm run tauri dev
```

The receiver alone needs none of the Windows tooling:

```sh
cargo build --release -p lanrec-recv
```

## Network setup

For the direct link, static addresses on both ends and no DHCP:

| | PC | Receiver |
|---|---|---|
| IP | 10.0.0.1/24 | 10.0.0.2/24 |
| MTU | 9000 | 9000 |

Jumbo frames noticeably reduce interrupt load at a few hundred megabits. Both
ends must be set to 9000 or it fragments.

## License

MIT — see [LICENSE](LICENSE). Third-party code is documented in
[NOTICE.md](NOTICE.md).
