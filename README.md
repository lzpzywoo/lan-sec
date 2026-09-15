# lan-sec

LAN remote desktop between **Windows** and **macOS**. Same binary is host or client.

Pipeline:

**Capture → Zero-copy (GPU) → Hardware Encode → BUD → Frame Timing → Hardware Decode**

True-color **HEVC 4:4:4** is used when both peers advertise hardware support. Otherwise the handshake falls back to HEVC 4:2:0 and logs that clearly.

## Commands

```
lansec probe
lansec host --bind 0.0.0.0:44700 --pin 1234
lansec client --connect 192.168.1.10:44700 --pin 1234
lansec loopback
```

## Platforms

| Role | Capture | Encode | Decode | Present | Input | Audio |
| --- | --- | --- | --- | --- | --- | --- |
| Windows | DXGI Desktop Duplication (GPU `CopyResource`, no CPU readback) | NVENC HEVC 4:4:4 (ARGB in, `chromaFormatIDC=3`) then QSV probe | D3D11VA / MF HEVC | DXGI flip-sequential | `SendInput` | WASAPI loopback + Opus |
| macOS | ScreenCaptureKit IOSurface | VideoToolbox HEVC (hardware required; 4:4:4 probed, else 4:2:0) | VideoToolbox | display-link style drop-late presenter | `CGEvent` | ScreenCaptureKit audio path + Opus |

## 4:4:4

- Windows host → Mac client is the primary true-color path (NVENC FREXT + VideoToolbox decode).
- Mac host 4:4:4 encode is probed with undocumented `HEVC_Main444_AutoLevel` **and** `RequireHardwareAcceleratedVideoEncoder`. If that fails, the host advertises 4:2:0 only. Software encode is never used.

## BUD

Custom UDP protocol (not Parsec's proprietary BUD):

- X25519 + HKDF-SHA256 + AES-256-GCM, PIN-bound
- Video: fragmented, unreliable; keyframes reliable
- Input/control: reliable + retransmit
- NACK / expire incomplete video frames → request IDR
- Congestion controller drives encoder bitrate

## Build

Windows: MSVC Build Tools + `cargo build -p lansec`

macOS: Xcode CLT + `cargo build -p lansec`

Grant Screen Recording on Mac. Allow UDP 44700 inbound on the host firewall.

Windows client decode uses the Media Foundation HEVC MFT (install **HEVC Video Extensions** from Microsoft if `lansec probe` warns it is missing).
