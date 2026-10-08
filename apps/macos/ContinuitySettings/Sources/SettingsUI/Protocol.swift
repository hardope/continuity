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

    public init(
        device: ThisDevice, paused: Bool, devices: [PairedDevice], nearby: [NearbyDevice],
        receivedFilesDir: String, canBeControlled: Bool, canBeUnlocked: Bool
    ) {
        self.device = device
        self.paused = paused
        self.devices = devices
        self.nearby = nearby
        self.receivedFilesDir = receivedFilesDir
        self.canBeControlled = canBeControlled
        self.canBeUnlocked = canBeUnlocked
    }
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
        case .android: "smartphone"
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
