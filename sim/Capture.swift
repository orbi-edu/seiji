// Framebuffer capture: SimulatorKit screen callbacks plus an IOSurface seed poll, frame pacing,
// GPU scaling into a pooled 4:2:0 buffer, then the encoder.
//
// Port discovery and callback registration follow serve-sim's FrameCapture.swift (Apache-2.0, Expo).

import CoreMedia
import CoreVideo
import Foundation
import IOSurface
import ObjectiveC
import VideoToolbox

typealias ScreenFrameCallback = @convention(block) () -> Void
typealias ScreenSurfacesChangedCallback = @convention(block) (IOSurface?, IOSurface?) -> Void
typealias ScreenPropertiesChangedCallback = @convention(block) (AnyObject?) -> Void

@objc protocol SimScreenDescriptor {
    @objc(registerScreenCallbacksWithUUID:callbackQueue:frameCallback:surfacesChangedCallback:propertiesChangedCallback:)
    func registerScreenCallbacks(
        uuid: UUID, callbackQueue: DispatchQueue,
        frameCallback: @escaping ScreenFrameCallback,
        surfacesChangedCallback: @escaping ScreenSurfacesChangedCallback,
        propertiesChangedCallback: @escaping ScreenPropertiesChangedCallback)
}

/// Everything runs on `queue`.
final class Capture {
    let queue = DispatchQueue(label: "seiji-sim.capture", qos: .userInteractive)

    private struct Port {
        let descriptor: NSObject
        let uuid: UUID
        var surface: IOSurface?
    }

    private static let pollInterval = DispatchTimeInterval.nanoseconds(1_000_000_000 / 60)
    private static let maxInFlight = 3
    /// Pace of retries for pending work (keyframe, new session) on an unchanged screen.
    private static let retryInterval: UInt64 = 200_000_000
    /// A decoder may hold the newest frames until more come: WebKit's holds 2 with one reference
    /// frame. When the screen goes still, the last picture is encoded this many times more (each
    /// a small frame with nothing changed), so every viewer shows it at once rather than at the
    /// next change.
    private static let repeats = 2

    private let io: NSObject
    private let pointScale: Double?
    private let encoder: Encoder
    private var fps: Int
    private var scale: Double
    private var ports: [Port] = []
    private var callbacks: [Any] = [] // keeps the registered blocks alive
    private var timer: DispatchSourceTimer?
    private var stopped = false
    /// Frames flow only after `run()`, so nothing reaches stdout before the `ready` status.
    private var running = false

    private var main: Int? // index into `ports` of the largest surface
    /// The surface and seed of the last frame handed to the encoder.
    private var lastSurface: IOSurface?
    private var lastSeed: UInt32?
    private var lastCaptureNanos: UInt64 = 0
    private var lastAttemptNanos: UInt64 = 0
    private var keyframeRequested = true
    /// Repeats of the last picture still to send (see `repeats`).
    private var repeatsOwed = 0
    /// The surface `sourceBuffer` wraps.
    private var bufferSurface: IOSurface?
    private var sourceBuffer: CVPixelBuffer?

    private var transfer: VTPixelTransferSession?
    private var pool: CVPixelBufferPool?
    private var poolSize = (width: 0, height: 0)

    init(device: NSObject, fps: Int, scale: Double, bitrate: Int) throws {
        guard let io = device.perform(NSSelectorFromString("io"))?.takeUnretainedValue() as? NSObject
        else { throw StartupError("the device has no IO client") }
        self.io = io
        self.pointScale = mainScreenScale(of: device)
        self.fps = fps
        self.scale = scale
        self.encoder = Encoder(fps: fps, bitrate: bitrate)
        var session: VTPixelTransferSession?
        guard VTPixelTransferSessionCreate(allocator: nil, pixelTransferSessionOut: &session) == noErr,
              let session else { throw StartupError("cannot create a pixel transfer session") }
        VTSessionSetProperty(session, key: kVTPixelTransferPropertyKey_ScalingMode,
                             value: kVTScalingMode_Normal)
        transfer = session
    }

    /// Registers the screen callbacks, waits for the main framebuffer and starts the encoder.
    /// Call from outside `queue`.
    func start(timeout: TimeInterval) throws {
        try queue.sync { try register() }
        let deadline = Date().addingTimeInterval(timeout)
        while queue.sync(execute: { pickMain() }) == nil {
            guard Date() < deadline else {
                stop()
                throw StartupError("no framebuffer surface appeared")
            }
            usleep(20_000)
        }
        try queue.sync {
            guard let index = main, let surface = ports[index].surface else {
                throw StartupError("no framebuffer surface")
            }
            try configureEncoder(for: surface)
            // The first GPU transfer takes ~300 ms; pay it before `ready`, not on the first frame.
            guard let source = pixelBuffer(for: surface), let output = scaledBuffer(), let transfer,
                  VTPixelTransferSessionTransferImage(transfer, from: source, to: output) == noErr
            else { throw StartupError("cannot scale the framebuffer") }
        }
    }

    /// Starts the poll and encodes the current screen as the first keyframe.
    func run() {
        queue.async { [self] in
            guard !stopped else { return }
            running = true
            let timer = DispatchSource.makeTimerSource(flags: .strict, queue: queue)
            timer.schedule(deadline: .now() + Self.pollInterval, repeating: Self.pollInterval,
                           leeway: .milliseconds(1))
            timer.setEventHandler { [weak self] in self?.tick(force: false) }
            timer.resume()
            self.timer = timer
            keyframeRequested = true
            tick(force: true)
        }
    }

    func requestKeyframe() {
        queue.async { [self] in
            keyframeRequested = true
            tick(force: true)
        }
    }

    func update(fps: Int?, scale: Double?, bitrate: Int?) {
        queue.async { [self] in
            guard !stopped else { return }
            if let bitrate { encoder.setBitrate(bitrate) }
            if let fps {
                self.fps = fps
                encoder.setFps(fps)
            }
            if let scale, scale != self.scale {
                self.scale = scale
                // The next capture re-creates the session at the new size; until one succeeds the
                // request stays pending, so a static screen still gets the new size.
                keyframeRequested = true
                tick(force: true)
            }
        }
    }

    /// Stops capture and unregisters the screen callbacks. Call from outside `queue`.
    func stop() {
        queue.sync {
            guard !stopped else { return }
            stopped = true
            running = false
            timer?.cancel()
            timer = nil
            let unregister = NSSelectorFromString("unregisterScreenCallbacksWithUUID:")
            for port in ports where port.descriptor.responds(to: unregister) {
                port.descriptor.perform(unregister, with: port.uuid)
            }
            ports.removeAll()
            callbacks.removeAll()
            encoder.invalidate()
            if let transfer { VTPixelTransferSessionInvalidate(transfer) }
            transfer = nil
        }
    }

    // MARK: - Ports

    private func register() throws {
        io.perform(NSSelectorFromString("updateIOPorts"))
        guard let ioPorts = io.value(forKey: "deviceIOPorts") as? [NSObject] else {
            throw StartupError("cannot read the device IO ports")
        }
        let identifierSel = NSSelectorFromString("portIdentifier")
        let descriptorSel = NSSelectorFromString("descriptor")
        let surfaceSel = NSSelectorFromString("framebufferSurface")
        let registerSel = #selector(SimScreenDescriptor.registerScreenCallbacks)
        for port in ioPorts {
            guard port.responds(to: identifierSel),
                  let identifier = port.perform(identifierSel)?.takeUnretainedValue(),
                  "\(identifier)" == "com.apple.framebuffer.display",
                  port.responds(to: descriptorSel),
                  let descriptor = port.perform(descriptorSel)?.takeUnretainedValue() as? NSObject,
                  descriptor.responds(to: surfaceSel), descriptor.responds(to: registerSel)
            else { continue }
            let index = ports.count
            let uuid = UUID()
            ports.append(Port(descriptor: descriptor, uuid: uuid, surface: nil))
            let frame: ScreenFrameCallback = { [weak self] in self?.tick(force: false) }
            let surfaces: ScreenSurfacesChangedCallback = { [weak self] unmasked, masked in
                guard let self, index < self.ports.count else { return }
                self.ports[index].surface = unmasked ?? masked
                self.main = nil
                self.tick(force: false)
            }
            let properties: ScreenPropertiesChangedCallback = { [weak self] _ in
                self?.main = nil
                self?.tick(force: false)
            }
            callbacks.append(contentsOf: [frame, surfaces, properties] as [Any])
            (descriptor as AnyObject).registerScreenCallbacks(
                uuid: uuid, callbackQueue: queue, frameCallback: frame,
                surfacesChangedCallback: surfaces, propertiesChangedCallback: properties)
        }
        guard !ports.isEmpty else { throw StartupError("no framebuffer display port") }
    }

    /// The largest-area surface. Cached until a surfaces/properties callback invalidates it.
    private func pickMain() -> Int? {
        if let main, ports[main].surface != nil { return main }
        let surfaceSel = NSSelectorFromString("framebufferSurface")
        var best: (index: Int, area: Int)?
        for index in ports.indices {
            if ports[index].surface == nil,
               let object = ports[index].descriptor.perform(surfaceSel)?.takeUnretainedValue() {
                ports[index].surface = unsafeBitCast(object, to: IOSurface.self)
            }
            guard let surface = ports[index].surface else { continue }
            let area = IOSurfaceGetWidth(surface) * IOSurfaceGetHeight(surface)
            if area > 0, area > (best?.area ?? 0) { best = (index, area) }
        }
        main = best?.index
        return main
    }

    // MARK: - Frames

    private func tick(force: Bool) {
        guard running, transfer != nil, let index = pickMain(), let surface = ports[index].surface
        else { return }
        let seed = IOSurfaceGetSeed(surface)
        let changed = surface !== lastSurface || seed != lastSeed
        // Work a static screen would never trigger again: a requested keyframe, one the
        // encoder or stdout dropped, or a session still to be created at a new size.
        let pending = keyframeRequested || encoder.needsKeyframe
        let repeating = !changed && !pending && repeatsOwed > 0
        guard changed || pending || force || repeating else { return }
        let now = monotonicNanos()
        // A keyframe line flood must not queue frames without bound: forced work over the limit
        // stays pending for a paced retry.
        guard encoder.framesInFlight < Self.maxInFlight else { return }
        if !force {
            // Encoding what stdout would refuse only wastes time; the change or pending work is
            // picked up by a later tick.
            guard Output.shared.acceptsFrames else { return }
            if changed || repeating {
                // At most `fps` frames a second.
                let interval = 1_000_000_000 / UInt64(fps)
                guard now &- lastCaptureNanos >= interval - interval / 8 else { return }
            } else {
                // Retry pending work on an unchanged screen at a slow pace, once the last attempt
                // has come back.
                guard encoder.framesInFlight == 0, now &- lastAttemptNanos >= Self.retryInterval
                else { return }
            }
        }
        lastAttemptNanos = now
        do {
            try configureEncoder(for: surface)
        } catch {
            debugLog("\(error)")
            return
        }
        guard let source = pixelBuffer(for: surface), let output = scaledBuffer(),
              let transfer,
              VTPixelTransferSessionTransferImage(transfer, from: source, to: output) == noErr
        else { return }
        lastSurface = surface
        lastSeed = seed
        lastCaptureNanos = now
        let keyframe = keyframeRequested
        keyframeRequested = false
        repeatsOwed = repeating ? repeatsOwed - 1 : Self.repeats
        encoder.encode(output, capturedMicros: unixMicros(), forceKeyframe: keyframe)
        debugLog("captured seed=\(seed) force=\(force) changed=\(changed) in \(Double(monotonicNanos() - now) / 1e6) ms")
    }

    private func configureEncoder(for surface: IOSurface) throws {
        let width = IOSurfaceGetWidth(surface)
        let height = IOSurfaceGetHeight(surface)
        func even(_ value: Int) -> Int { max(2, Int(Double(value) * scale) & ~1) }
        let screen = pointScale.map {
            (width: Int((Double(width) / $0).rounded()), height: Int((Double(height) / $0).rounded()))
        }
        try encoder.configure(width: even(width), height: even(height), screen: screen)
    }

    /// A CVPixelBuffer over the framebuffer surface, kept for as long as the surface lasts.
    private func pixelBuffer(for surface: IOSurface) -> CVPixelBuffer? {
        if surface === bufferSurface, let sourceBuffer { return sourceBuffer }
        var made: Unmanaged<CVPixelBuffer>?
        guard CVPixelBufferCreateWithIOSurface(kCFAllocatorDefault, surface, nil, &made)
            == kCVReturnSuccess, let buffer = made?.takeRetainedValue() else { return nil }
        bufferSurface = surface
        sourceBuffer = buffer
        return buffer
    }

    private func scaledBuffer() -> CVPixelBuffer? {
        let size = (width: encoder.width, height: encoder.height)
        if pool == nil || poolSize != size {
            let attributes: [CFString: Any] = [
                kCVPixelBufferPixelFormatTypeKey: kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
                kCVPixelBufferWidthKey: size.width,
                kCVPixelBufferHeightKey: size.height,
                kCVPixelBufferIOSurfacePropertiesKey: [CFString: Any](),
            ]
            pool = nil
            CVPixelBufferPoolCreate(nil, nil, attributes as CFDictionary, &pool)
            poolSize = size
        }
        guard let pool else { return nil }
        var buffer: CVPixelBuffer?
        CVPixelBufferPoolCreatePixelBuffer(nil, pool, &buffer)
        return buffer
    }
}
