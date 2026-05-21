# DWE exploreHD H.264 1080p30 hotfix

## TL;DR

The exploreHD's hardware H.264 encoder drops whole frames at 1080p30 (~30 %
loss at the default 10 Mbps setting). When it drops a frame mid-GOP its next
P-frame still references the frame it never put on the wire, so the
receiver's decoder paints garbage against the wrong reference and the
corruption smears through the rest of the GOP -- this is the visible
"glitch" the customer sees.

**The fix is to shrink the GOP, not to raise the bitrate.**

- `--gop 0` -- every frame is a keyframe (all-intra). Drops become scalar
  (a missing frame is just a missing frame, never a corrupted neighbour).
  Visually perfect. Wire cost ~40 Mbps at 1080p30.
- `--gop 1` -- alternating IP (one P-frame between I-frames). Worst case is
  one corrupted P-frame. Wire cost ~26 Mbps at 1080p30.
- `--gop 29` (factory default) -- one I-frame per ~30 frames. Every drop
  poisons up to 29 subsequent frames at the receiver -- this is what the
  customer is seeing now.

Bitrate matters only to keep per-I-frame quality reasonable -- it does not
prevent the corruption mechanism. `--mode 1` (CBR) is recommended; VBR mode
clamps internally on this firmware and under-utilises the budget.

The setting can be applied **while the camera is already streaming** -- no
MCM / BlueOS / camera restart needed -- and persists until the camera is
unplugged or the Pi reboots.

## Apply

On the vehicle (BlueOS host SSH):

```bash
# Copy the script onto the Pi
scp tools/dwe/dwe_xu_set.py pi@<vehicle>:/usr/local/bin/dwe-xu-set
ssh pi@<vehicle> "sudo chmod +x /usr/local/bin/dwe-xu-set"

# Apply the fix mid-stream (autodetects the exploreHD H.264 node)
ssh pi@<vehicle> "sudo /usr/local/bin/dwe-xu-set --gop 0"    # all-intra, ~40 Mbps
# or, if downlink-constrained:
ssh pi@<vehicle> "sudo /usr/local/bin/dwe-xu-set --gop 1"    # IPIPIP, ~26 Mbps
```

Watch Cockpit: the 1080p30 H.264 stream should clear up within a second
(the next keyframe reseeds the decoder).

## Persistence

* Survives `kraken` container restarts and MCM restarts (camera holds the
  setpoint in its firmware while powered).
* **Does NOT** survive a USB unplug / Pi reboot. After a reboot the camera
  reverts to its `gop=29` default.
* For a permanent fix, deploy a small systemd unit (or, ideally, integrate
  the XU write into MCM startup -- see "MCM integration" below).

## Permanent (systemd / udev) deployment

Drop this on the vehicle as `/etc/systemd/system/dwe-xu-tune.service`:

```ini
[Unit]
Description=Tune DWE exploreHD H.264 encoder for 1080p30 glitch-free
After=network.target

[Service]
Type=oneshot
ExecStart=/usr/local/bin/dwe-xu-set --gop 0 --mode 1
RemainAfterExit=yes

[Install]
WantedBy=multi-user.target
```

A udev rule that fires on the exploreHD's USB add event is more robust
(handles hot-plug). Drop as `/etc/udev/rules.d/99-dwe-xu-tune.rules`:

```
SUBSYSTEM=="video4linux", ACTION=="add", ATTRS{idVendor}=="0c45", ATTRS{idProduct}=="6366", \
  RUN+="/usr/local/bin/dwe-xu-set --gop 0 --mode 1"
```

(Update `idVendor` / `idProduct` for other DWE PIDs; see `PID_VIDS` in
dweOS for the full list -- the `0x3961` family is also exploreHD-class.)

## MCM integration (proper fix)

Integrate the same protocol into `src/lib/video/video_source_local.rs`:

1. After detecting a DWE camera (match against `PID_VIDS` extracted from
   dweOS), open the H.264 sub-node (the `natsorted` index 2 of the
   camera's `/dev/videoN` paths).
2. Issue the same two `UVCIOC_CTRL_QUERY/SET_CUR` ioctls.
3. Expose `--dwe-h264-gop` (default `0`, all-intra) and optionally
   `--dwe-h264-bitrate-mbps` and `--dwe-h264-mode` for tunability.

That removes the need for a separate udev / systemd unit and lets users
tune from Cockpit.

## Protocol reference

Documented in the script's module docstring (`dwe_xu_set.py`). Sources:

* `DeepWaterExploration/dweOS:backend_py/src/services/cameras/xu_controls.py`
* `DeepWaterExploration/dweOS:backend_py/src/services/cameras/ehd.py`
* `DeepWaterExploration/dweOS:backend_py/src/services/cameras/camera_helper/camera_helper.c`

Encoder GOP semantics (important):  on this firmware, the `gop` value is
**the number of P-frames between keyframes**, not the GOP length. So:

| `--gop` | Pattern | Meaning |
|---------|---------|---------|
| 0 | IIII... | All-intra (every frame a keyframe) |
| 1 | IPIPIP... | Alternating; one P between each pair of I's |
| 29 | IPPP...P | One I-frame followed by 29 P-frames (factory default) |

## Empirical envelope

Stock exploreHD, kernel 6.6.31, BlueOS 1.4.4, 30-second captures at
1080p30 H.264 via `v4l2-ctl --stream-mmap` (raw NALs to file, then
ffprobe NAL-type histogram):

| Configuration              | NAL composition | Real fps | Wire bitrate | Visual |
|----------------------------|-----------------|----------|--------------|--------|
| `gop=29 br=10M` (default)  | 42 I + 842 P    | **20.5** | 7.2 Mbps     | Glitchy (P-frame corruption smears through GOP) |
| `gop=1 br=10M`             | 468 I + 432 P   | 27.8     | 26.0 Mbps    | Clean (smear bounded to 1 P-frame per drop) |
| **`gop=0 br=10M`**         | 900 I + 0 P     | **29.0** | **40.9 Mbps**| **Clean (no inter-frame deps; like MJPG)** |

Notes:

- `gop=0` and `gop=1` *exceed* the configured bitrate cap. The encoder
  cannot fit all-keyframe (or near-all-keyframe) content in 10 Mbps so it
  just emits what it needs. Setting a higher `--bitrate` only raises the
  per-I-frame quality budget; it doesn't reduce the wire cost.
- There is a hardware throughput ceiling around 28-29 fps at 1080p H.264
  on this encoder silicon. No setting reaches a sustained 30 fps.
- For 30 fps without any glitches, use 1280x720 H.264 or 1920x1080 MJPG;
  both deliver a clean 30 fps from this camera.

## Why the v4l2-ctl raw-capture test still "looked clean" to ffprobe

`ffprobe -err_detect explode` flags **bitstream parse errors** (bad NAL
headers, missing escape bytes, etc.). It does **not** flag "this P-frame
references a frame that was never delivered" -- the decoder happily emits
the wrong picture from the wrong reference, which is what you see as the
glitch. The structural integrity of the NAL stream is preserved even when
the inter-frame semantic chain is broken; the artefact is only visible
when the stream is actually rendered. This is why the bench-only signal
("more frames at higher bitrate = better fps") was right about fps but
wrong about visual quality.
