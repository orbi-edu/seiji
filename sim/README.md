# seiji-sim

A small Swift helper the Seiji daemon runs once per watched iOS lease. It captures the simulator's
main framebuffer, encodes it to H.264 with VideoToolbox, and writes framed `format` / `frame` /
`status` messages to stdout. It reads JSON-line input commands (touch, keys, buttons, keyframe and
encoding changes) from stdin and sends them over one persistent SimulatorKit HID connection.

It loads CoreSimulator and SimulatorKit with `dlopen` at run time, so it needs Xcode on the host
but no link-time reference to the private frameworks.

```
seiji-sim session --udid <UDID> --fps <1-60> --scale <0.1-1> --bitrate <bits per second>
seiji-sim --version
```

Set `SEIJI_SIM_DEBUG=1` to log timings (touch line to HID send, capture to frame) on stderr.

## Build

From the repository root, on macOS with Xcode:

```
swiftc -O -swift-version 5 -o target/seiji-sim sim/*.swift -framework VideoToolbox -framework CoreMedia -framework CoreVideo -framework IOSurface -framework Accelerate
```

## Sources

| File | Concern |
|---|---|
| `main.swift` | CLI, startup, signals, shutdown |
| `Frameworks.swift` | Developer dir, framework loading, device lookup |
| `Capture.swift` | Screen callbacks, IOSurface poll, frame pacing, scaling |
| `Encoder.swift` | VTCompressionSession, avcC, `format` and `frame` messages |
| `HID.swift` | SimDeviceLegacyHIDClient: touch, keyboard, buttons |
| `Protocol.swift` | stdout framing, stdin line reader |
| `Commands.swift` | stdin command parsing and validation |

Parts are informed by [serve-sim](https://github.com/expo/serve-sim) (Apache-2.0, Expo) and
[idb](https://github.com/facebook/idb) (MIT, Meta); files that follow them say so at the top.
