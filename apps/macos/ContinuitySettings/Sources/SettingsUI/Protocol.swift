import Foundation

/// What `continuityd` reports — the JSON its control socket sends
/// (`Status` in core/continuityd/src/control.rs), decoded with
/// snake_case keys converted.
public struct Status: Decodable, Equatable {
    public var device: ThisDevice
    public var paused: Bool
    public var devices: [PairedDevice]
    public var nearby: [NearbyDevice]
    public var receivedFilesDir: String
    public var canBeControlled: Bool
    public var canBeUnlocked: Bool
    // Optional so a window newer than the running continuityd still opens.
    public var about: About?
    public var activity: Activity?
    /// macOS privacy permissions, as continuityd itself has them.
    public var permissions: Permissions?

    public init(
        device: ThisDevice, paused: Bool, devices: [PairedDevice], nearby: [NearbyDevice],
        receivedFilesDir: String, canBeControlled: Bool, canBeUnlocked: Bool,
        about: About? = nil, activity: Activity? = nil, permissions: Permissions? = nil
    ) {
        self.device = device
        self.paused = paused
        self.devices = devices
        self.nearby = nearby
        self.receivedFilesDir = receivedFilesDir
        self.canBeControlled = canBeControlled
        self.canBeUnlocked = canBeUnlocked
        self.about = about
        self.activity = activity
        self.permissions = permissions
    }

    /// The permissions remote control of this Mac still needs, in the
    /// order they're asked for.
    public var missingPermissions: [Permission] {
        guard canBeControlled, let permissions else { return [] }
        return Permission.allCases.filter { !permissions.allows($0) }
    }
}

/// What the info panel shows beyond the device itself.
public struct About: Decodable, Equatable {
    public var protocolVersion: UInt32
    /// "macOS 12.7.6".
    public var os: String
    public var startedAtUnix: UInt64
    public var logFile: String?
    public var configDir: String?

    public init(protocolVersion: UInt32, os: String, startedAtUnix: UInt64, logFile: String?, configDir: String?) {
        self.protocolVersion = protocolVersion
        self.os = os
        self.startedAtUnix = startedAtUnix
        self.logFile = logFile
        self.configDir = configDir
    }
}

/// Counted since Continuity started.
public struct Activity: Decodable, Equatable {
    public var connections: UInt64
    public var clipboardSent: UInt64
    public var clipboardReceived: UInt64
    public var filesSent: UInt64
    public var bytesSent: UInt64
    public var filesReceived: UInt64
    public var bytesReceived: UInt64
    public var remoteControlSessions: UInt64
    public var screenLocks: UInt64

    public init(
        connections: UInt64, clipboardSent: UInt64, clipboardReceived: UInt64, filesSent: UInt64, bytesSent: UInt64,
        filesReceived: UInt64, bytesReceived: UInt64, remoteControlSessions: UInt64, screenLocks: UInt64
    ) {
        self.connections = connections
        self.clipboardSent = clipboardSent
        self.clipboardReceived = clipboardReceived
        self.filesSent = filesSent
        self.bytesSent = bytesSent
        self.filesReceived = filesReceived
        self.bytesReceived = bytesReceived
        self.remoteControlSessions = remoteControlSessions
        self.screenLocks = screenLocks
    }
}

public struct Permissions: Decodable, Equatable {
    public var screenRecording: Bool
    public var accessibility: Bool

    public init(screenRecording: Bool, accessibility: Bool) {
        self.screenRecording = screenRecording
        self.accessibility = accessibility
    }

    public func allows(_ permission: Permission) -> Bool {
        switch permission {
        case .screenRecording: screenRecording
        case .accessibility: accessibility
        }
    }
}

/// A macOS privacy permission remote control needs. The raw value is what
/// `request_permission` takes.
public enum Permission: String, CaseIterable, Identifiable {
    case screenRecording = "screen_recording"
    case accessibility

    public var id: String { rawValue }
}

public struct ThisDevice: Decodable, Equatable {
    public var name: String
    public var id: String
    public var platform: Platform
    public var version: String

    public init(name: String, id: String, platform: Platform, version: String) {
        self.name = name
        self.id = id
        self.platform = platform
        self.version = version
    }
}

public struct PairedDevice: Decodable, Equatable, Identifiable {
    public var id: String
    public var name: String
    /// Known once the device has connected (or been seen nearby) since
    /// Continuity started.
    public var platform: Platform?
    public var connected: Bool
    public var pairedAtUnix: UInt64
    public var remoteControlAllowed: Bool
    public var unlockAllowed: Bool

    public init(
        id: String, name: String, platform: Platform?, connected: Bool, pairedAtUnix: UInt64,
        remoteControlAllowed: Bool, unlockAllowed: Bool
    ) {
        self.id = id
        self.name = name
        self.platform = platform
        self.connected = connected
        self.pairedAtUnix = pairedAtUnix
        self.remoteControlAllowed = remoteControlAllowed
        self.unlockAllowed = unlockAllowed
    }
}

public struct NearbyDevice: Decodable, Equatable, Identifiable {
    public var id: String
    public var name: String
    public var platform: Platform

    public init(id: String, name: String, platform: Platform) {
        self.id = id
        self.name = name
        self.platform = platform
    }
}

public enum Platform: String, Decodable, Equatable {
    case macOS = "mac_os"
    case windows
    case linux
    case android
    case iOS = "ios"
    /// Something newer than this window knows about.
    case unknown

    public init(from decoder: Decoder) throws {
        let raw = try decoder.singleValueContainer().decode(String.self)
        self = Platform(rawValue: raw) ?? .unknown
    }

    public var isPhone: Bool { self == .android || self == .iOS }

    var displayName: String {
        switch self {
        case .macOS: "Mac"
        case .windows: "Windows PC"
        case .linux: "Linux computer"
        case .android: "Android phone"
        case .iOS: "iPhone"
        case .unknown: "Device"
        }
    }

    var symbol: String {
        switch self {
        case .macOS: "laptopcomputer"
        case .windows: "pc"
        case .linux: "desktopcomputer"
        // "smartphone" is new in macOS 14; earlier, it draws nothing.
        case .android:
            if #available(macOS 14.0, *) { "smartphone" } else { "iphone" }
        case .iOS: "iphone"
        case .unknown: "display"
        }
    }
}

/// Every response line: `ok`, plus a `status` (status and watch) or a
/// `message` (a refused change).
struct Envelope: Decodable {
    var ok: Bool
    var message: String?
    var status: Status?

    static func decode(_ line: Data) throws -> Envelope {
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        return try decoder.decode(Envelope.self, from: line)
    }
}
