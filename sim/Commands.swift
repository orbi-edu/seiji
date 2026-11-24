// stdin commands: one JSON object per line.

import Foundation

struct Limits {
    static let fps = 1...60
    static let scale = 0.1...1.0
    static let bitrate = 10_000...200_000_000
}

final class Commands {
    private let hid: HID
    private let capture: Capture

    init(hid: HID, capture: Capture) {
        self.hid = hid
        self.capture = capture
    }

    /// Runs one line; a problem with it becomes a `status` error and the line is ignored.
    func handle(_ line: Data) {
        let received = unixMicros()
        if let error = run(line, received: received) { Output.shared.error(error) }
    }

    private func run(_ line: Data, received: UInt64) -> String? {
        guard let parsed = try? JSONSerialization.jsonObject(with: line),
              let object = parsed as? [String: Any] else { return "bad JSON line" }
        if let phase = object["touch"] {
            return touch(object, phase: phase, received: received)
        }
        if let usage = object["key"] {
            guard only(object, ["key", "down"]) else { return "unknown key in key line" }
            guard let usage = integer(usage), (0...255).contains(usage) else {
                return "key must be a HID usage 0-255"
            }
            guard let down = boolean(object["down"]) else { return "key needs \"down\": true|false" }
            return hid.key(usage: UInt32(usage), down: down)
        }
        if let name = object["button"] {
            guard only(object, ["button", "down"]) else { return "unknown key in button line" }
            let button: HID.Button
            switch name as? String {
            case "home": button = .home
            case "lock": button = .lock
            default: return "button must be \"home\" or \"lock\""
            }
            guard let down = boolean(object["down"]) else { return "button needs \"down\": true|false" }
            return hid.button(button, down: down)
        }
        if let keyframe = object["keyframe"] {
            guard only(object, ["keyframe"]), boolean(keyframe) == true else {
                return "keyframe line must be {\"keyframe\":true}"
            }
            capture.requestKeyframe()
            return nil
        }
        if let encode = object["encode"] {
            guard only(object, ["encode"]), let settings = encode as? [String: Any] else {
                return "encode must be an object"
            }
            return self.encode(settings)
        }
        return "unknown command"
    }

    private func touch(_ object: [String: Any], phase: Any, received: UInt64) -> String? {
        guard only(object, ["touch", "x", "y", "edge"]) else { return "unknown key in touch line" }
        let touchPhase: HID.TouchPhase
        switch phase as? String {
        case "down": touchPhase = .down
        case "move": touchPhase = .move
        case "up": touchPhase = .up
        default: return "touch must be \"down\", \"move\" or \"up\""
        }
        guard let x = number(object["x"]), let y = number(object["y"]),
              (0...1).contains(x), (0...1).contains(y) else { return "touch x and y must be 0-1" }
        var edge = HID.Edge.none
        if let name = object["edge"] {
            // Only a down's edge is used; the gesture keeps it until its up.
            switch name as? String {
            case "top": edge = .top
            case "bottom": edge = .bottom
            case "left": edge = .left
            case "right": edge = .right
            default: return "edge must be \"top\", \"bottom\", \"left\" or \"right\""
            }
        }
        let error = hid.touch(touchPhase, x: x, y: y, edge: edge)
        if debugEnabled {
            let sent = unixMicros()
            debugLog("touch \(phase) received_us=\(received) sent_us=\(sent) parse+send=\(sent - received) us")
        }
        return error
    }

    private func encode(_ settings: [String: Any]) -> String? {
        guard only(settings, ["fps", "scale", "bitrate"]) else { return "unknown key in encode" }
        var fps: Int?
        var scale: Double?
        var bitrate: Int?
        if let value = settings["fps"] {
            guard let value = integer(value), Limits.fps.contains(value) else {
                return "fps must be an integer 1-60"
            }
            fps = value
        }
        if let value = settings["scale"] {
            guard let value = number(value), Limits.scale.contains(value) else {
                return "scale must be 0.1-1"
            }
            scale = value
        }
        if let value = settings["bitrate"] {
            guard let value = integer(value), Limits.bitrate.contains(value) else {
                return "bitrate must be an integer \(Limits.bitrate.lowerBound)-\(Limits.bitrate.upperBound)"
            }
            bitrate = value
        }
        capture.update(fps: fps, scale: scale, bitrate: bitrate)
        return nil
    }

    private func only(_ object: [String: Any], _ allowed: Set<String>) -> Bool {
        object.keys.allSatisfy(allowed.contains)
    }
}

// JSON values: NSNumber carries both booleans and numbers, so tell them apart by CF type.

func boolean(_ value: Any?) -> Bool? {
    guard let number = value as? NSNumber, CFGetTypeID(number) == CFBooleanGetTypeID() else { return nil }
    return number.boolValue
}

func number(_ value: Any?) -> Double? {
    guard let number = value as? NSNumber, CFGetTypeID(number) != CFBooleanGetTypeID() else { return nil }
    let double = number.doubleValue
    return double.isFinite ? double : nil
}

func integer(_ value: Any?) -> Int? {
    guard let double = number(value), double == double.rounded(), abs(double) < 1e15 else { return nil }
    return Int(double)
}
