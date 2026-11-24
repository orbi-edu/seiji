// Loading the private simulator frameworks and finding the device.
//
// CoreSimulator and SimulatorKit are dlopen'd and used through the Objective-C runtime only, so the
// binary has no load command whose location depends on the Xcode version. Informed by serve-sim's
// SimFrameworks.swift and FrameCapture.findSimDevice (Apache-2.0, Expo).

import Foundation
import ObjectiveC

struct StartupError: Error {
    let message: String
    init(_ message: String) { self.message = message }
}

enum SimFrameworks {
    /// `DEVELOPER_DIR` if set, else `xcode-select -p`, else the default Xcode location.
    static func developerDir() -> String {
        if let dir = ProcessInfo.processInfo.environment["DEVELOPER_DIR"], !dir.isEmpty {
            return dir
        }
        let process = Process()
        process.executableURL = URL(fileURLWithPath: "/usr/bin/xcode-select")
        process.arguments = ["-p"]
        let pipe = Pipe()
        process.standardOutput = pipe
        process.standardError = FileHandle.nullDevice
        process.standardInput = FileHandle.nullDevice
        if (try? process.run()) != nil {
            let data = pipe.fileHandleForReading.readDataToEndOfFile()
            process.waitUntilExit()
            if process.terminationStatus == 0,
               let path = String(data: data, encoding: .utf8)?
                   .trimmingCharacters(in: .whitespacesAndNewlines),
               !path.isEmpty {
                return path
            }
        }
        return "/Applications/Xcode.app/Contents/Developer"
    }

    static func load(developerDir dev: String) throws {
        let coreSimulator = "/Library/Developer/PrivateFrameworks/CoreSimulator.framework/CoreSimulator"
        guard dlopen(coreSimulator, RTLD_NOW) != nil else {
            throw StartupError("cannot load CoreSimulator: \(lastDlError())")
        }
        let simulatorKit = [
            "\(dev)/../SharedFrameworks/SimulatorKit.framework/SimulatorKit", // Xcode 27+
            "\(dev)/Library/PrivateFrameworks/SimulatorKit.framework/SimulatorKit", // older
        ]
        guard simulatorKit.contains(where: { dlopen($0, RTLD_NOW) != nil }) else {
            throw StartupError("cannot load SimulatorKit from \(dev): \(lastDlError())")
        }
    }

    private static func lastDlError() -> String {
        guard let error = dlerror() else { return "unknown error" }
        return String(cString: error)
    }
}

/// The booted `SimDevice` with this UDID.
func findBootedDevice(udid: String, developerDir: String) throws -> NSObject {
    guard let contextClass = NSClassFromString("SimServiceContext") as? NSObject.Type else {
        throw StartupError("SimServiceContext not found in CoreSimulator")
    }
    let sharedSel = NSSelectorFromString("sharedServiceContextForDeveloperDir:error:")
    guard let context = contextClass.perform(sharedSel, with: developerDir, with: nil)?
        .takeUnretainedValue() as? NSObject else {
        throw StartupError("cannot open the CoreSimulator service for \(developerDir)")
    }
    let setSel = NSSelectorFromString("defaultDeviceSetWithError:")
    guard let deviceSet = context.perform(setSel, with: nil)?.takeUnretainedValue() as? NSObject,
          let devices = deviceSet.value(forKey: "devices") as? [NSObject] else {
        throw StartupError("cannot read the default device set")
    }
    let wanted = udid.uppercased()
    guard let device = devices.first(where: {
        ($0.value(forKey: "UDID") as? NSUUID)?.uuidString == wanted
    }) else {
        throw StartupError("device \(udid) not found")
    }
    let state = device.value(forKey: "stateString") as? String ?? "unknown"
    guard state == "Booted" else {
        throw StartupError("device \(udid) is not booted (state: \(state))")
    }
    return device
}

/// The device's main screen scale (pixels per point), if CoreSimulator reports it.
func mainScreenScale(of device: NSObject) -> Double? {
    guard let type = device.value(forKey: "deviceType") as? NSObject,
          let scale = type.value(forKey: "mainScreenScale") as? NSNumber,
          scale.doubleValue > 0 else { return nil }
    return scale.doubleValue
}
