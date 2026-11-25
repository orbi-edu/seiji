// H.264 encoding with VideoToolbox, and the `format` / `frame` messages it produces.
//
// Session setup and the avcC layout follow serve-sim's H264Encoder.swift (Apache-2.0, Expo).

import CoreMedia
import CoreVideo
import Foundation
import VideoToolbox

/// Driven from the capture queue. VideoToolbox calls the output handler on its own threads; the
/// state it shares with `encode` is behind `lock`.
final class Encoder {
    /// What one session encodes. Each submitted frame carries its session's copy, so a frame
    /// finishing while the session is replaced still describes itself correctly.
    private struct Stream {
        let generation: Int
        let width: Int
        let height: Int
        /// Screen size in points, for the `format` message.
        let screen: (width: Int, height: Int)?
    }

    private var fps: Int
    private var bitrate: Int
    private var session: VTCompressionSession?
    private var stream: Stream?
    private var generation = 0

    private let lock = NSLock()
    private var inFlight = 0
    private var formatGeneration = -1
    private var lastDescription: Data?
    /// Set after a dropped frame (or before the first format): deltas are useless until an IDR.
    private var needKeyframe = true
    /// The last keyframe was over the payload limit; set until a keyframe is written or the
    /// session changes, so an unchanged screen does not keep encoding one that is too big again.
    private var keyframeTooBig = false
    /// Encoded frames not written (stdout backlog or over the payload limit).
    private var dropped = 0
    /// Flags, capture time and encode time before each sample.
    private static let frameHeader = 17

    init(fps: Int, bitrate: Int) {
        self.fps = fps
        self.bitrate = bitrate
    }

    deinit { invalidate() }

    /// The size of the current session's pictures (0 before the first `configure`).
    var width: Int { stream?.width ?? 0 }
    var height: Int { stream?.height ?? 0 }

    var framesInFlight: Int {
        lock.lock()
        defer { lock.unlock() }
        return inFlight
    }

    /// True while a keyframe of a working session is owed and worth retrying on an unchanged
    /// screen. Changed frames are still sent as keyframes until one is written.
    var needsKeyframe: Bool {
        lock.lock()
        defer { lock.unlock() }
        return (needKeyframe && !keyframeTooBig) || session == nil
    }

    /// (Re)creates the session if the output size or screen changed. The next frame is then an
    /// IDR and is preceded by a new `format`.
    func configure(width: Int, height: Int, screen: (width: Int, height: Int)?) throws {
        if session != nil, let stream, width == stream.width, height == stream.height,
           screen?.width == stream.screen?.width, screen?.height == stream.screen?.height { return }
        invalidate()
        lock.lock()
        needKeyframe = true
        keyframeTooBig = false
        lock.unlock()
        generation += 1
        stream = Stream(generation: generation, width: width, height: height, screen: screen)
        session = try makeSession(width: width, height: height)
        debugLog("encoder session \(generation): \(width)x\(height) fps=\(fps) bitrate=\(bitrate)")
    }

    func setBitrate(_ value: Int) {
        bitrate = value
        guard let session else { return }
        set(session, kVTCompressionPropertyKey_AverageBitRate, NSNumber(value: value))
        set(session, kVTCompressionPropertyKey_DataRateLimits, dataRateLimits())
    }

    func setFps(_ value: Int) {
        fps = value
        guard let session else { return }
        set(session, kVTCompressionPropertyKey_ExpectedFrameRate, NSNumber(value: value))
    }

    func invalidate() {
        guard let session else { return }
        VTCompressionSessionCompleteFrames(session, untilPresentationTimeStamp: .invalid)
        VTCompressionSessionInvalidate(session)
        self.session = nil
    }

    func encode(_ buffer: CVPixelBuffer, capturedMicros: UInt64, forceKeyframe: Bool) {
        guard let session, let stream else { return }
        lock.lock()
        let keyframe = forceKeyframe || needKeyframe
        inFlight += 1
        lock.unlock()
        let properties: CFDictionary? = keyframe
            ? [kVTEncodeFrameOptionKey_ForceKeyFrame: kCFBooleanTrue!] as CFDictionary : nil
        let status = VTCompressionSessionEncodeFrame(
            session, imageBuffer: buffer,
            presentationTimeStamp: CMClockGetTime(CMClockGetHostTimeClock()),
            duration: .invalid, frameProperties: properties, infoFlagsOut: nil
        ) { [weak self] status, flags, sample in
            self?.output(status: status, flags: flags, sample: sample, stream: stream,
                         capturedMicros: capturedMicros, requestedKeyframe: keyframe)
        }
        if status != noErr {
            lock.lock()
            inFlight -= 1
            needKeyframe = true
            lock.unlock()
            debugLog("VTCompressionSessionEncodeFrame failed: \(status)")
        }
    }

    // MARK: - Output

    private func output(status: OSStatus, flags: VTEncodeInfoFlags, sample: CMSampleBuffer?,
                        stream: Stream, capturedMicros: UInt64, requestedKeyframe: Bool) {
        lock.lock()
        defer { lock.unlock() }
        inFlight -= 1
        // The rate control skipped this frame: nothing refers to it, so the stream goes on. Only
        // a keyframe that was asked for is still owed. (Forcing one after every skip makes things
        // worse at a low bitrate: each keyframe is skipped or makes the next frame skip, and the
        // stream turns into keyframes only.)
        if status == noErr && flags.contains(.frameDropped) {
            if requestedKeyframe { needKeyframe = true }
            debugLog("the encoder skipped a frame\(requestedKeyframe ? " (a keyframe)" : "")")
            return
        }
        guard status == noErr, let sample, let block = CMSampleBufferGetDataBuffer(sample) else {
            if status != noErr { debugLog("encode failed: \(status)") }
            needKeyframe = true
            return
        }
        let keyframe = Self.isKeyframe(sample)
        if keyframe, let format = CMSampleBufferGetFormatDescription(sample),
           let description = Self.avcC(format) {
            if stream.generation != formatGeneration || description != lastDescription {
                guard sendFormat(description, stream: stream) else {
                    needKeyframe = true
                    return
                }
                formatGeneration = stream.generation
                lastDescription = description
            }
        }
        // No format for this session yet, or waiting for an IDR after a drop.
        guard stream.generation == formatGeneration, keyframe || !needKeyframe else {
            needKeyframe = true
            return
        }

        let length = CMBlockBufferGetDataLength(block)
        guard Self.frameHeader + length <= Output.maxPayload else {
            // Never written. A delta loss still needs an IDR, but a keyframe this big would most
            // likely be too big again, so a static screen does not retry it (see `needsKeyframe`).
            dropped += 1
            needKeyframe = true
            if keyframe { keyframeTooBig = true }
            debugLog("dropping a \(length)-byte \(keyframe ? "key" : "delta") frame over the \(Output.maxPayload)-byte payload limit (\(dropped) dropped)")
            return
        }
        // Flags (bit 0: keyframe; bit 1: the encode time follows), the capture time, the time the
        // encoder handed the frame over, then the sample.
        var message = Output.message(.frame, payloadLength: Self.frameHeader + length)
        message.append((keyframe ? 1 : 0) | 2)
        for time in [capturedMicros, unixMicros()] {
            for shift in stride(from: 56, through: 0, by: -8) {
                message.append(UInt8(truncatingIfNeeded: time >> UInt64(shift)))
            }
        }
        let header = message.count
        message.count = header + length
        let copied = message.withUnsafeMutableBytes { raw in
            CMBlockBufferCopyDataBytes(block, atOffset: 0, dataLength: length,
                                       destination: raw.baseAddress! + header)
        }
        guard copied == kCMBlockBufferNoErr else {
            needKeyframe = true
            return
        }
        if Output.shared.send(message, droppable: true) {
            if keyframe {
                needKeyframe = false
                keyframeTooBig = false
            }
        } else {
            dropped += 1
            needKeyframe = true
            debugLog("stdout backlog: dropped a \(keyframe ? "key" : "delta") frame (\(dropped) dropped)")
            return
        }
        debugLog("frame \(keyframe ? "key" : "delta") \(length) B, capture→queued \(Double(unixMicros() - capturedMicros) / 1000) ms")
    }

    private func sendFormat(_ description: Data, stream: Stream) -> Bool {
        let sps = description.subdata(in: 1..<4)
        let codec = "avc1." + sps.map { String(format: "%02X", $0) }.joined()
        var object: [String: Any] = [
            "codec": codec,
            "width": stream.width,
            "height": stream.height,
            "description": description.base64EncodedString(),
        ]
        if let screen = stream.screen {
            object["screen"] = ["width": screen.width, "height": screen.height]
        }
        guard let json = try? JSONSerialization.data(withJSONObject: object, options: [.sortedKeys])
        else { return false }
        var message = Output.message(.format, payloadLength: json.count)
        message.append(json)
        debugLog("format \(String(decoding: json, as: UTF8.self))")
        return Output.shared.send(message)
    }

    private static func isKeyframe(_ sample: CMSampleBuffer) -> Bool {
        guard let attachments = CMSampleBufferGetSampleAttachmentsArray(sample, createIfNecessary: false)
            as? [[CFString: Any]], let first = attachments.first else { return true }
        return (first[kCMSampleAttachmentKey_NotSync] as? Bool) != true
    }

    /// AVCDecoderConfigurationRecord (ISO/IEC 14496-15 5.2.4.1) with every SPS and PPS.
    private static func avcC(_ format: CMFormatDescription) -> Data? {
        var count = 0
        var nalHeaderLength: Int32 = 0
        guard CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
            format, parameterSetIndex: 0, parameterSetPointerOut: nil, parameterSetSizeOut: nil,
            parameterSetCountOut: &count, nalUnitHeaderLengthOut: &nalHeaderLength) == noErr,
            count >= 2, nalHeaderLength == 4 else { return nil }
        var sps: [Data] = []
        var pps: [Data] = []
        for index in 0..<count {
            var pointer: UnsafePointer<UInt8>?
            var size = 0
            guard CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                format, parameterSetIndex: index, parameterSetPointerOut: &pointer,
                parameterSetSizeOut: &size, parameterSetCountOut: nil,
                nalUnitHeaderLengthOut: nil) == noErr, let pointer, size > 0, size <= 0xFFFF
            else { return nil }
            let set = Data(bytes: pointer, count: size)
            switch set[0] & 0x1F {
            case 7: sps.append(set)
            case 8: pps.append(set)
            default: break
            }
        }
        guard let first = sps.first, first.count >= 4, !pps.isEmpty, sps.count <= 31,
              pps.count <= 255 else { return nil }
        var record = Data([1, first[1], first[2], first[3], 0xFC | 3, 0xE0 | UInt8(sps.count)])
        for set in sps {
            record.append(UInt8(set.count >> 8)); record.append(UInt8(set.count & 0xFF))
            record.append(set)
        }
        record.append(UInt8(pps.count))
        for set in pps {
            record.append(UInt8(set.count >> 8)); record.append(UInt8(set.count & 0xFF))
            record.append(set)
        }
        // High profiles (100+) carry chroma format and bit depth; 4:2:0 8-bit here.
        if [100, 110, 122, 144].contains(first[1]) {
            record.append(contentsOf: [0xFC | 1, 0xF8, 0xF8, 0])
        }
        return record
    }

    // MARK: - Session

    private func makeSession(width: Int, height: Int) throws -> VTCompressionSession {
        var created: VTCompressionSession?
        func create(_ spec: [CFString: Any]) -> OSStatus {
            VTCompressionSessionCreate(
                allocator: kCFAllocatorDefault, width: Int32(width), height: Int32(height),
                codecType: kCMVideoCodecType_H264, encoderSpecification: spec as CFDictionary,
                imageBufferAttributes: nil, compressedDataAllocator: nil, outputCallback: nil,
                refcon: nil, compressionSessionOut: &created)
        }
        // Low-latency rate control also makes the stream signal a small decoder buffer, so a
        // decoder outputs each frame at once instead of filling its DPB first.
        var status = create([
            kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder: true,
            kVTVideoEncoderSpecification_EnableLowLatencyRateControl: true,
        ])
        if status != noErr || created == nil {
            debugLog("low-latency encoder unavailable (\(status)); using the default one")
            created = nil
            status = create([kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder: true])
        }
        guard status == noErr, let session = created else {
            throw StartupError("cannot create the H.264 encoder at \(width)x\(height) (\(status))")
        }
        set(session, kVTCompressionPropertyKey_RealTime, kCFBooleanTrue!)
        set(session, kVTCompressionPropertyKey_AllowFrameReordering, kCFBooleanFalse!)
        if !set(session, kVTCompressionPropertyKey_ProfileLevel, kVTProfileLevel_H264_High_AutoLevel) {
            set(session, kVTCompressionPropertyKey_ProfileLevel, kVTProfileLevel_H264_Main_AutoLevel)
        }
        set(session, kVTCompressionPropertyKey_AverageBitRate, NSNumber(value: bitrate))
        set(session, kVTCompressionPropertyKey_DataRateLimits, dataRateLimits())
        set(session, kVTCompressionPropertyKey_ExpectedFrameRate, NSNumber(value: fps))
        // Viewers ask for a keyframe when they need one (joining, falling behind, a lost frame),
        // so a periodic one is only a safety net: on a slow link each keyframe is a burst that
        // delays the frames behind it.
        set(session, kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration, NSNumber(value: 10))
        let prepared = VTCompressionSessionPrepareToEncodeFrames(session)
        guard prepared == noErr else {
            VTCompressionSessionInvalidate(session)
            throw StartupError("cannot start the H.264 encoder (\(prepared))")
        }
        return session
    }

    private func dataRateLimits() -> CFArray {
        [NSNumber(value: Double(bitrate) * 1.5 / 8), NSNumber(value: 1)] as CFArray
    }

    @discardableResult
    private func set(_ session: VTCompressionSession, _ key: CFString, _ value: CFTypeRef) -> Bool {
        let status = VTSessionSetProperty(session, key: key, value: value)
        if status != noErr { debugLog("encoder property \(key) refused: \(status)") }
        return status == noErr
    }
}
