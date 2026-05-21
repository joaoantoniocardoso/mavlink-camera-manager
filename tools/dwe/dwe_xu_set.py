#!/usr/bin/env python3
"""DWE exploreHD / stellarHD UVC Extension Unit poke.

Reads / writes the H.264 bitrate, GOP and CBR/VBR mode on DeepWaterExploration
cameras *while the camera is streaming* (no service restart needed).

Protocol
--------
* Open the H.264 v4l2 node (typically the 3rd ``/dev/videoN`` of the camera
  when the device paths are natsorted, i.e. ``cameras[2]``).
* Issue **two** ``UVCIOC_CTRL_QUERY / UVC_SET_CUR`` ioctls on
  ``unit=4`` (``USR_ID``), ``selector=2`` (``USR_H264_CTRL``), ``size=11``:

  1. switch packet: ``[0x9A, <cmd>, 0, 0, 0, 0, 0, 0, 0, 0, 0]``
     (``0x9A`` = ``DWE_DEVICE_TAG``; ``<cmd>`` selects which sub-control)
  2. value packet: payload bytes in ``[0..N-1]``, zeros in ``[N..10]``

* Commands (``<cmd>`` byte):

  ===========================  =====  ==========================================
  Name                         Cmd    Payload
  ===========================  =====  ==========================================
  ``H264_BITRATE_CTRL``        0x02   big-endian uint32, bits/second
  ``GOP_CTRL``                 0x03   native uint16, frames
  ``H264_MODE_CTRL``           0x06   uint8, 1 = CBR, 2 = VBR
  ===========================  =====  ==========================================

"""

from __future__ import annotations

import argparse
import ctypes
import fcntl
import os
import struct
import sys

UVC_SET_CUR = 0x01
UVC_GET_CUR = 0x81

DWE_DEVICE_TAG = 0x9A
USR_UNIT = 4
USR_H264_CTRL = 2
PAYLOAD_SIZE = 11

CMD_BITRATE = 0x02
CMD_GOP = 0x03
CMD_MODE = 0x06

MODE_CBR = 1
MODE_VBR = 2


class UvcXuQ(ctypes.Structure):
    """``struct uvc_xu_control_query`` from ``<linux/uvcvideo.h>``."""

    _fields_ = [
        ("unit", ctypes.c_uint8),
        ("selector", ctypes.c_uint8),
        ("query", ctypes.c_uint8),
        ("size", ctypes.c_uint16),
        ("data", ctypes.POINTER(ctypes.c_uint8)),
    ]


def _ioctl_nr() -> int:
    """Compute ``UVCIOC_CTRL_QUERY`` for the local pointer width.

    ``_IOWR('u', 0x21, sizeof(struct uvc_xu_control_query))``.

    The struct's size is 12 on 32-bit userspace and 16 on 64-bit userspace
    (1+1+1 + 1pad + 2 + Npad + sizeof(ptr)).  Compute at runtime so the
    same script runs on either bitness.
    """

    return (3 << 30) | (ctypes.sizeof(UvcXuQ) << 16) | (ord("u") << 8) | 0x21


def _xu(fd: int, query: int, data: list[int]) -> bytes:
    arr = (ctypes.c_uint8 * PAYLOAD_SIZE)(*data)
    q = UvcXuQ(
        unit=USR_UNIT,
        selector=USR_H264_CTRL,
        query=query,
        size=PAYLOAD_SIZE,
        data=ctypes.cast(arr, ctypes.POINTER(ctypes.c_uint8)),
    )
    fcntl.ioctl(fd, _ioctl_nr(), q)
    return bytes(arr)


def _switch(fd: int, cmd: int) -> None:
    _xu(fd, UVC_SET_CUR, [DWE_DEVICE_TAG, cmd] + [0] * (PAYLOAD_SIZE - 2))


def set_bitrate(fd: int, bps: int) -> None:
    _switch(fd, CMD_BITRATE)
    payload = list(struct.pack(">I", bps)) + [0] * (PAYLOAD_SIZE - 4)
    _xu(fd, UVC_SET_CUR, payload)


def get_bitrate(fd: int) -> int:
    _switch(fd, CMD_BITRATE)
    arr = (ctypes.c_uint8 * PAYLOAD_SIZE)()
    q = UvcXuQ(
        unit=USR_UNIT,
        selector=USR_H264_CTRL,
        query=UVC_GET_CUR,
        size=PAYLOAD_SIZE,
        data=ctypes.cast(arr, ctypes.POINTER(ctypes.c_uint8)),
    )
    fcntl.ioctl(fd, _ioctl_nr(), q)
    return struct.unpack(">I", bytes(arr)[:4])[0]


def set_gop(fd: int, gop: int) -> None:
    _switch(fd, CMD_GOP)
    payload = list(struct.pack("H", gop)) + [0] * (PAYLOAD_SIZE - 2)
    _xu(fd, UVC_SET_CUR, payload)


def set_mode(fd: int, mode: int) -> None:
    _switch(fd, CMD_MODE)
    _xu(fd, UVC_SET_CUR, [mode] + [0] * (PAYLOAD_SIZE - 1))


def autodetect_h264_node() -> str:
    """Return the first UVC ``/dev/videoN`` that advertises H.264 capture.

    Filters out platform encoders (e.g. ``bcm2835-codec``, ``rpivid``) by
    requiring ``driver=uvcvideo`` -- those expose H.264 too but only as a
    memory-to-memory codec, not a real camera with an XU.
    """

    import glob
    import os
    import subprocess

    candidates: list[str] = []
    for node in sorted(glob.glob("/dev/video*")):
        name = os.path.basename(node)
        driver_link = f"/sys/class/video4linux/{name}/device/driver"
        try:
            driver = os.path.basename(os.readlink(driver_link))
        except OSError:
            continue
        if driver != "uvcvideo":
            continue
        try:
            out = subprocess.check_output(
                ["v4l2-ctl", "--device", node, "--list-formats"],
                stderr=subprocess.DEVNULL,
                text=True,
            )
        except (subprocess.CalledProcessError, FileNotFoundError):
            continue
        if "H264" in out.upper():
            candidates.append(node)

    if not candidates:
        raise SystemExit(
            "no UVC /dev/videoN advertises H264; pass --device explicitly "
            "(typically /dev/video4 on exploreHD)"
        )
    return candidates[0]


def main(argv: list[str] | None = None) -> int:
    p = argparse.ArgumentParser(
        description="Set DWE exploreHD H.264 bitrate / GOP / mode via UVC XU. "
        "Safe to run mid-stream; takes effect within one frame.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""\
recommended one-liners for the 1080p30 H.264 glitch fix:
  sudo %(prog)s --gop 0          # all-intra; visually perfect; ~40 Mbps wire
  sudo %(prog)s --gop 1          # IPIPIP; glitches bounded to 1 frame; ~26 Mbps wire

note: on this firmware "gop" = number of P-frames between keyframes
      (not GOP length).  gop=0 is all-intra.  gop=29 is the broken default.
""",
    )
    p.add_argument(
        "--device",
        help="H.264 v4l2 node (default: autodetect; usually /dev/video4)",
    )
    p.add_argument("--bitrate", type=int, help="encoder target, bits per second")
    p.add_argument("--gop", type=int, help="group-of-pictures size, frames")
    p.add_argument(
        "--mode",
        type=int,
        choices=[MODE_CBR, MODE_VBR],
        help="1 = CBR (recommended), 2 = VBR (under-fills on this firmware)",
    )
    p.add_argument(
        "--get",
        action="store_true",
        help="print current bitrate readback and exit",
    )
    args = p.parse_args(argv)

    device = args.device or autodetect_h264_node()
    print(f"using device: {device}")

    try:
        fd = os.open(device, os.O_RDWR)
    except OSError as error:
        sys.stderr.write(f"open({device!r}) failed: {error}\n")
        if error.errno == 13:
            sys.stderr.write("hint: run with sudo, or add yourself to the 'video' group\n")
        return 1

    nothing_to_set = (
        args.bitrate is None and args.gop is None and args.mode is None
    )
    try:
        if args.get or nothing_to_set:
            cur = get_bitrate(fd)
            print(f"current bitrate: {cur} bps  ({cur / 1e6:.2f} Mbps)")
            if args.get:
                return 0

        if args.bitrate is not None:
            set_bitrate(fd, args.bitrate)
            cur = get_bitrate(fd)
            ok = "OK" if cur == args.bitrate else "MISMATCH"
            print(
                f"set bitrate -> {args.bitrate} bps  ({args.bitrate / 1e6:.2f} Mbps); "
                f"readback {cur} ({cur / 1e6:.2f} Mbps) [{ok}]"
            )

        if args.mode is not None:
            set_mode(fd, args.mode)
            name = {MODE_CBR: "CBR", MODE_VBR: "VBR"}[args.mode]
            print(f"set mode    -> {args.mode} ({name})")

        if args.gop is not None:
            set_gop(fd, args.gop)
            print(f"set gop     -> {args.gop} frames")

        return 0
    finally:
        os.close(fd)


if __name__ == "__main__":
    sys.exit(main())
