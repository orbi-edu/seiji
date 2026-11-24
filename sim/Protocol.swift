// stdout framing and stdin JSON lines.

import Foundation

let debugEnabled = ProcessInfo.processInfo.environment["SEIJI_SIM_DEBUG"] == "1"

func debugLog(_ message: @autoclosure () -> String) {
    guard debugEnabled else { return }
    FileHandle.standardError.write(Data("seiji-sim[debug]: \(message())\n".utf8))
}

/// Unix time in microseconds.
func unixMicros() -> UInt64 {
    var ts = timespec()
    clock_gettime(CLOCK_REALTIME, &ts)
    return UInt64(ts.tv_sec) * 1_000_000 + UInt64(ts.tv_nsec) / 1_000
}

/// Monotonic nanoseconds.
func monotonicNanos() -> UInt64 {
    clock_gettime_nsec_np(CLOCK_UPTIME_RAW)
}

/// Framed messages on stdout: `kind: u8`, `length: u32 BE`, payload. One serial queue owns fd 1, so
/// messages never interleave and callers never wait on the pipe.
final class Output {
    enum Kind: UInt8 {
        case format = 1
        case frame = 2
        case status = 3
    }

    static let shared = Output()
    static let maxPayload = 8 << 20
    /// Queued bytes above which droppable messages (frames) are refused.
    private static let maxBacklog = 4 << 20

    private let queue = DispatchQueue(label: "seiji-sim.stdout", qos: .userInitiated)
    private let lock = NSLock()
    private var backlog = 0
    private var broken = false
    private var closed = false

    /// A message buffer with the 5-byte header filled in, ready for `payloadLength` payload bytes.
    static func message(_ kind: Kind, payloadLength: Int) -> Data {
        var data = Data(capacity: 5 + payloadLength)
        let length = UInt32(payloadLength)
        data.append(kind.rawValue)
        data.append(UInt8(truncatingIfNeeded: length >> 24))
        data.append(UInt8(truncatingIfNeeded: length >> 16))
        data.append(UInt8(truncatingIfNeeded: length >> 8))
        data.append(UInt8(truncatingIfNeeded: length))
        return data
    }

    /// Queues a whole message built by `message(_:payloadLength:)`. Returns false if it was
    /// refused: output closed or broken, or `droppable` and the reader is too far behind.
    @discardableResult
    func send(_ message: Data, droppable: Bool = false) -> Bool {
        guard message.count - 5 <= Self.maxPayload else {
            debugLog("dropping a \(message.count)-byte message over the payload limit")
            return false
        }
        lock.lock()
        if closed || broken || (droppable && backlog > Self.maxBacklog) {
            lock.unlock()
            return false
        }
        backlog += message.count
        lock.unlock()
        queue.async { [self] in
            let ok = writeAll(message)
            lock.lock()
            backlog -= message.count
            if !ok { broken = true }
            lock.unlock()
        }
        return true
    }

    func status(_ object: [String: Any]) {
        guard let json = try? JSONSerialization.data(withJSONObject: object) else { return }
        var message = Self.message(.status, payloadLength: json.count)
        message.append(json)
        send(message)
    }

    func error(_ text: String) {
        debugLog("status error: \(text)")
        status(["error": text])
    }

    /// Stops accepting messages and waits (bounded) for the queued ones to reach the pipe.
    func closeAndDrain(timeout: DispatchTimeInterval) {
        lock.lock()
        closed = true
        lock.unlock()
        let done = DispatchSemaphore(value: 0)
        queue.async { done.signal() }
        _ = done.wait(timeout: .now() + timeout)
    }

    private func writeAll(_ data: Data) -> Bool {
        data.withUnsafeBytes { raw -> Bool in
            guard var pointer = raw.baseAddress else { return true }
            var remaining = raw.count
            while remaining > 0 {
                let written = write(STDOUT_FILENO, pointer, remaining)
                if written < 0 {
                    if errno == EINTR { continue }
                    if errno == EAGAIN {
                        var pfd = pollfd(fd: STDOUT_FILENO, events: Int16(POLLOUT), revents: 0)
                        _ = poll(&pfd, 1, 100)
                        continue
                    }
                    return false
                }
                pointer += written
                remaining -= written
            }
            return true
        }
    }
}

/// Reads `\n`-terminated JSON lines from stdin on its own thread and hands each to `onLine`.
/// Calls `onEOF` once stdin closes.
final class LineReader {
    static let maxLine = 64 * 1024

    private let onLine: (Data) -> Void
    private let onTooLong: () -> Void
    private let onEOF: () -> Void

    init(onLine: @escaping (Data) -> Void, onTooLong: @escaping () -> Void,
         onEOF: @escaping () -> Void) {
        self.onLine = onLine
        self.onTooLong = onTooLong
        self.onEOF = onEOF
    }

    func start() {
        let thread = Thread { [self] in run() }
        thread.name = "seiji-sim.stdin"
        thread.qualityOfService = .userInteractive
        thread.start()
    }

    private func run() {
        var pending = Data()
        var discarding = false // inside a line that went over the limit
        let chunkSize = 64 * 1024
        let chunk = UnsafeMutableRawPointer.allocate(byteCount: chunkSize, alignment: 1)
        defer { chunk.deallocate() }
        while true {
            let count = read(STDIN_FILENO, chunk, chunkSize)
            if count < 0 {
                if errno == EINTR { continue }
                break
            }
            if count == 0 { break }
            var bytes = UnsafeRawBufferPointer(start: chunk, count: count)[...]
            while let newline = bytes.firstIndex(of: 0x0A) {
                let head = bytes[bytes.startIndex..<newline]
                bytes = bytes[(newline + 1)...]
                if discarding {
                    discarding = false
                    pending.removeAll(keepingCapacity: true)
                    continue
                }
                if pending.count + head.count > Self.maxLine {
                    pending.removeAll(keepingCapacity: true)
                    onTooLong()
                    continue
                }
                pending.append(contentsOf: head)
                onLine(pending)
                pending.removeAll(keepingCapacity: true)
            }
            if discarding { continue }
            if pending.count + bytes.count > Self.maxLine {
                pending.removeAll(keepingCapacity: true)
                discarding = true
                onTooLong()
            } else {
                pending.append(contentsOf: bytes)
            }
        }
        onEOF()
    }
}
