// seiji-sim: lends a booted iOS simulator's screen and HID to the Seiji daemon.
// See sim/README.md and the contract in the daemon's docs.

import Foundation

let version = "0.1.0"

func fail(_ message: String) -> Never {
    FileHandle.standardError.write(Data("seiji-sim: \(message)\n".utf8))
    exit(1)
}

struct SessionOptions {
    var udid = ""
    var fps = 0
    var scale = 0.0
    var bitrate = 0
}

func parseSession(_ args: ArraySlice<String>) -> SessionOptions {
    var options = SessionOptions()
    var seen = Set<String>()
    var rest = args
    while let flag = rest.popFirst() {
        guard let value = rest.popFirst() else { fail("\(flag) needs a value") }
        switch flag {
        case "--udid":
            guard UUID(uuidString: value) != nil else { fail("--udid must be a UDID") }
            options.udid = value
        case "--fps":
            guard let fps = Int(value), Limits.fps.contains(fps) else { fail("--fps must be 1-60") }
            options.fps = fps
        case "--scale":
            guard let scale = Double(value), Limits.scale.contains(scale) else {
                fail("--scale must be 0.1-1")
            }
            options.scale = scale
        case "--bitrate":
            guard let bitrate = Int(value), Limits.bitrate.contains(bitrate) else {
                fail("--bitrate must be \(Limits.bitrate.lowerBound)-\(Limits.bitrate.upperBound)")
            }
            options.bitrate = bitrate
        default:
            fail("unknown option \(flag)")
        }
        seen.insert(flag)
    }
    for flag in ["--udid", "--fps", "--scale", "--bitrate"] where !seen.contains(flag) {
        fail("missing \(flag)")
    }
    return options
}

/// Lifts a finger still down, unregisters the screen callbacks, flushes stdout, exits 0.
final class Shutdown {
    private let lock = NSLock()
    private var started = false
    private let hid: HID
    private let capture: Capture

    init(hid: HID, capture: Capture) {
        self.hid = hid
        self.capture = capture
    }

    func run(_ reason: String) -> Never {
        lock.lock()
        if started {
            lock.unlock()
            // Another thread is already cleaning up and will exit.
            while true { pause() }
        }
        started = true
        lock.unlock()
        debugLog("exiting: \(reason)")
        hid.liftFinger(timeout: .milliseconds(200))
        capture.stop()
        Output.shared.closeAndDrain(timeout: .milliseconds(300))
        exit(0)
    }
}

func session(_ options: SessionOptions) -> Never {
    signal(SIGPIPE, SIG_IGN)
    let developerDir = SimFrameworks.developerDir()
    let hid: HID
    let capture: Capture
    do {
        try SimFrameworks.load(developerDir: developerDir)
        let device = try findBootedDevice(udid: options.udid, developerDir: developerDir)
        hid = try HID(device: device, onError: { Output.shared.error($0) })
        capture = try Capture(device: device, fps: options.fps, scale: options.scale,
                              bitrate: options.bitrate)
        try capture.start(timeout: 5)
    } catch let error as StartupError {
        fail(error.message)
    } catch {
        fail("\(error)")
    }

    let shutdown = Shutdown(hid: hid, capture: capture)
    let signalQueue = DispatchQueue(label: "seiji-sim.signals")
    var sources: [DispatchSourceSignal] = []
    for number in [SIGTERM, SIGINT] {
        signal(number, SIG_IGN)
        let source = DispatchSource.makeSignalSource(signal: number, queue: signalQueue)
        source.setEventHandler { shutdown.run("signal \(number)") }
        source.resume()
        sources.append(source)
    }

    Output.shared.status(["ready": true])
    capture.run()
    let commands = Commands(hid: hid, capture: capture)
    LineReader(
        onLine: { commands.handle($0) },
        onTooLong: { Output.shared.error("line longer than \(LineReader.maxLine) bytes") },
        onEOF: { shutdown.run("stdin closed") }
    ).start()
    withExtendedLifetime(sources) { dispatchMain() }
}

let arguments = CommandLine.arguments.dropFirst()
switch arguments.first {
case "--version":
    print("seiji-sim \(version)")
    exit(0)
case "session":
    session(parseSession(arguments.dropFirst()))
default:
    fail("usage: seiji-sim session --udid <UDID> --fps <1-60> --scale <0.1-1> --bitrate <bps> | seiji-sim --version")
}
