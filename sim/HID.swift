// Persistent HID connection to the simulator: touch, keyboard and hardware buttons.
//
// Uses SimulatorKit's SimDeviceLegacyHIDClient and the Indigo message builders, as in serve-sim's
// HIDInjector.swift (Apache-2.0, Expo) and idb's FBSimulatorHID (MIT, Meta).

import CoreGraphics
import Foundation
import ObjectiveC

final class HID {
    enum Edge: UInt32 {
        case none = 0, left = 1, top = 2, bottom = 3, right = 4
    }

    enum Button: Int32 {
        case home = 0x0, lock = 0x1
    }

    private typealias InitFn = @convention(c) (
        AnyObject, Selector, AnyObject, AutoreleasingUnsafeMutablePointer<NSError?>
    ) -> AnyObject?
    private typealias SendFn = @convention(c) (
        AnyObject, Selector, UnsafeMutableRawPointer, ObjCBool, AnyObject?, AnyObject?
    ) -> Void
    private typealias MouseFn = @convention(c) (
        UnsafePointer<CGPoint>, UnsafePointer<CGPoint>?, UInt32, Int32, CGFloat, CGFloat, UInt32
    ) -> UnsafeMutableRawPointer?
    private typealias KeyboardFn = @convention(c) (UInt32, UInt32) -> UnsafeMutableRawPointer?
    private typealias ButtonFn = @convention(c) (Int32, Int32, Int32) -> UnsafeMutableRawPointer?
    private typealias Completion = @convention(block) (NSError?) -> Void

    private static let touchTarget: UInt32 = 0x32
    private static let buttonTarget: Int32 = 0x33
    private static let touchDown: Int32 = 1 // also used for moves
    private static let touchUp: Int32 = 2

    private let client: AnyObject
    private let sendSel = NSSelectorFromString("sendWithMessage:freeWhenDone:completionQueue:completion:")
    private let sendFn: SendFn
    private let mouseFn: MouseFn
    private let keyboardFn: KeyboardFn
    private let buttonFn: ButtonFn
    private let completionQueue = DispatchQueue(label: "seiji-sim.hid-completion")
    private let reportFailure: Completion

    /// Serializes sends: the stdin thread and shutdown both use the client.
    private let lock = NSLock()
    private var finger: (point: CGPoint, edge: Edge)?
    /// Set by `close`: later input is not sent, so nothing can press after the final lift.
    private var closed = false

    init(device: NSObject, onError: @escaping (String) -> Void) throws {
        guard let hidClass = NSClassFromString("_TtC12SimulatorKit24SimDeviceLegacyHIDClient") else {
            throw StartupError("SimDeviceLegacyHIDClient not found in SimulatorKit")
        }
        let initSel = NSSelectorFromString("initWithDevice:error:")
        guard let initImp = class_getMethodImplementation(hidClass, initSel) else {
            throw StartupError("SimDeviceLegacyHIDClient has no initWithDevice:error:")
        }
        var error: NSError?
        let made = unsafeBitCast(initImp, to: InitFn.self)(
            class_createInstance(hidClass, 0) as AnyObject, initSel, device, &error)
        guard let made, error == nil else {
            throw StartupError("cannot connect HID: \(error?.localizedDescription ?? "no client")")
        }
        guard let sendImp = class_getMethodImplementation(object_getClass(made), sendSel) else {
            throw StartupError("SimDeviceLegacyHIDClient has no sendWithMessage:")
        }
        func symbol(_ name: String) throws -> UnsafeMutableRawPointer {
            guard let pointer = dlsym(UnsafeMutableRawPointer(bitPattern: -2), name) else {
                throw StartupError("\(name) not found")
            }
            return pointer
        }
        client = made
        sendFn = unsafeBitCast(sendImp, to: SendFn.self)
        mouseFn = unsafeBitCast(try symbol("IndigoHIDMessageForMouseNSEvent"), to: MouseFn.self)
        keyboardFn = unsafeBitCast(try symbol("IndigoHIDMessageForKeyboardArbitrary"), to: KeyboardFn.self)
        buttonFn = unsafeBitCast(try symbol("IndigoHIDMessageForButton"), to: ButtonFn.self)
        reportFailure = { error in
            if let error { onError("HID send failed: \(error.localizedDescription)") }
        }
    }

    enum TouchPhase { case down, move, up }

    /// Returns an error message for a touch that does not fit the finger's state.
    func touch(_ phase: TouchPhase, x: Double, y: Double, edge: Edge) -> String? {
        lock.lock()
        defer { lock.unlock() }
        guard !closed else { return nil }
        let point = CGPoint(x: x, y: y)
        switch phase {
        case .down:
            // A lost `up` must not wedge the finger: lift it where it was, then start again.
            if let old = finger { sendTouch(Self.touchUp, old.point, old.edge) }
            finger = (point, edge)
            return sendTouch(Self.touchDown, point, edge)
        case .move:
            guard let old = finger else { return "touch move without a finger down" }
            finger = (point, old.edge)
            return sendTouch(Self.touchDown, point, old.edge)
        case .up:
            guard let old = finger else { return "touch up without a finger down" }
            finger = nil
            return sendTouch(Self.touchUp, point, old.edge)
        }
    }

    func key(usage: UInt32, down: Bool) -> String? {
        lock.lock()
        defer { lock.unlock() }
        guard !closed else { return nil }
        return send(keyboardFn(usage, down ? 1 : 2), what: "keyboard")
    }

    func button(_ button: Button, down: Bool) -> String? {
        lock.lock()
        defer { lock.unlock() }
        guard !closed else { return nil }
        return send(buttonFn(button.rawValue, down ? 1 : 2, Self.buttonTarget), what: "button")
    }

    /// Stops sending input, then lifts a finger still down at its last point and waits (bounded)
    /// for that send to finish. Both happen under the input lock, so a touch either went out
    /// before (and is lifted here) or is never sent.
    func close(timeout: DispatchTimeInterval) {
        lock.lock()
        defer { lock.unlock() }
        closed = true
        guard let old = finger else { return }
        finger = nil
        var point = old.point
        guard let message = mouseFn(&point, nil, Self.touchTarget, Self.touchUp, 1, 1,
                                    old.edge.rawValue) else { return }
        let done = DispatchSemaphore(value: 0)
        let completion: Completion = { _ in done.signal() }
        sendFn(client, sendSel, message, true, completionQueue, completion as AnyObject)
        let lifted = done.wait(timeout: .now() + timeout) == .success
        debugLog("lifted the finger at (\(point.x), \(point.y))\(lifted ? "" : " (no completion in time)")")
    }

    @discardableResult
    private func sendTouch(_ type: Int32, _ point: CGPoint, _ edge: Edge) -> String? {
        var p = point
        return send(mouseFn(&p, nil, Self.touchTarget, type, 1, 1, edge.rawValue), what: "touch")
    }

    private func send(_ message: UnsafeMutableRawPointer?, what: String) -> String? {
        guard let message else { return "cannot build the \(what) HID message" }
        sendFn(client, sendSel, message, true, completionQueue, reportFailure as AnyObject)
        return nil
    }
}
