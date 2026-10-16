import Foundation
import Network
import XCTest
@testable import SettingsUI

/// The Continuity window's side of the control protocol, against a fake
/// `continuityd` listening on a real loopback socket. Fixtures/status.json
/// is the same file core/continuityd/src/control.rs checks its own output
/// against.
final class ControlClientTests: XCTestCase {
    private func fixture() throws -> Data {
        let url = try XCTUnwrap(Bundle.module.url(forResource: "status", withExtension: "json", subdirectory: "Fixtures"))
        return try Data(contentsOf: url)
    }

    func testDecodesWhatContinuitydSends() throws {
        let status = try XCTUnwrap(Envelope.decode(fixture()).status)
        XCTAssertEqual(status.device.platform, .macOS)
        XCTAssertEqual(status.devices.map(\.name), ["Pixel 8", "Office PC"])
        XCTAssertEqual(status.devices[0].platform, .android)
        XCTAssertTrue(status.devices[0].remoteControlAllowed)
        XCTAssertNil(status.devices[1].platform, "not seen since Continuity started")
        XCTAssertEqual(status.nearby.first?.platform, .android)
        XCTAssertEqual(status.receivedFilesDir, "/Users/me/Downloads/Continuity")
        XCTAssertTrue(status.canBeControlled)
        XCTAssertFalse(status.canBeUnlocked)
        XCTAssertEqual(status.about?.os, "macOS 12.7.6")
        XCTAssertEqual(status.about?.protocolVersion, 2)
        XCTAssertEqual(status.activity?.bytesSent, 3_500_000)
        XCTAssertEqual(status.activity?.remoteControlSessions, 1)
        XCTAssertEqual(status.permissions, Permissions(screenRecording: false, accessibility: true))
        XCTAssertEqual(status.missingPermissions, [.screenRecording])
    }

    func testAContinuitydWithoutTheInfoFieldsStillDecodes() throws {
        var json = try XCTUnwrap(JSONSerialization.jsonObject(with: fixture()) as? [String: Any])
        var status = try XCTUnwrap(json["status"] as? [String: Any])
        for key in ["about", "activity", "permissions"] {
            status.removeValue(forKey: key)
        }
        json["status"] = status
        let decoded = try XCTUnwrap(Envelope.decode(JSONSerialization.data(withJSONObject: json)).status)
        XCTAssertNil(decoded.about)
        XCTAssertEqual(decoded.missingPermissions, [], "nothing to warn about without knowing")
    }

    func testDecodesTheTransfers() throws {
        let status = try XCTUnwrap(Envelope.decode(fixture()).status)
        XCTAssertEqual(status.canControlOthers, true)
        XCTAssertNil(status.controllingDevice)
        let transfers = try XCTUnwrap(status.transfers)
        XCTAssertEqual(transfers.map(\.id), ["t4", "t3", "t2", "t1"], "newest first, as sent")
        XCTAssertEqual(transfers.map(\.state), [.active, .stalled, .done, .failed])
        XCTAssertEqual(
            transfers[0],
            Transfer(
                id: "t4", direction: .sending, fileName: "talk.mov", peerName: "Pixel 8", sizeBytes: 1_300_000_000,
                bytesDone: 650_000_000, state: .active, reason: nil, path: nil, startedAtUnix: 1_760_000_400, finishedAtUnix: nil
            )
        )
        XCTAssertEqual(transfers[0].fraction, 0.5, accuracy: 0.0001)
        XCTAssertEqual(transfers[1].direction, .receiving)
        XCTAssertEqual(transfers[1].reason, "Lost the connection to Pixel 8")
        XCTAssertEqual(transfers[2].path, "/Users/me/Downloads/Continuity/IMG_2041.jpg")
        XCTAssertEqual(transfers[2].finishedAtUnix, 1_760_000_102)
        XCTAssertEqual(transfers[3].sizeBytes, 5_000_000_000, "bigger than 32 bits")
        XCTAssertEqual(transfers[3].reason, "Not enough free space on Pixel 8 (needs 5 GB, 300 MB free)")
        XCTAssertEqual(transfers.map(\.canBeOpened), [false, false, true, false], "only a received file that's done")
        XCTAssertEqual(transfers.map(\.canBeCancelled), [true, true, false, false], "only one still moving")
        let pixel = try XCTUnwrap(status.devices.first { $0.name == "Pixel 8" })
        let office = try XCTUnwrap(status.devices.first { $0.name == "Office PC" })
        XCTAssertEqual(status.transfers(with: pixel).map(\.id), ["t4", "t3", "t2", "t1"], "moving first, then the last few finished")
        XCTAssertEqual(status.transfers(with: office).map(\.id), [])
    }

    func testAContinuitydWithoutTransfersStillDecodes() throws {
        var json = try XCTUnwrap(JSONSerialization.jsonObject(with: fixture()) as? [String: Any])
        var status = try XCTUnwrap(json["status"] as? [String: Any])
        for key in ["transfers", "can_control_others"] {
            status.removeValue(forKey: key)
        }
        json["status"] = status
        let decoded = try XCTUnwrap(Envelope.decode(JSONSerialization.data(withJSONObject: json)).status)
        XCTAssertNil(decoded.transfers)
        XCTAssertNil(decoded.canControlOthers)
        XCTAssertEqual(decoded.devices.map(\.name), ["Pixel 8", "Office PC"])
        XCTAssertEqual(decoded.receivedFilesDir, "/Users/me/Downloads/Continuity")
        let mac = PairedDevice(
            id: "mac", name: "iMac", platform: .macOS, connected: true, pairedAtUnix: 0, remoteControlAllowed: false, unlockAllowed: false
        )
        XCTAssertFalse(decoded.canControl(mac), "no Control button without knowing this build has the viewer")
    }

    func testOnlyAConnectedComputerCanBeControlled() throws {
        var status = try XCTUnwrap(Envelope.decode(fixture()).status)
        func device(_ platform: Platform?, connected: Bool = true) -> PairedDevice {
            PairedDevice(
                id: "other", name: "Other", platform: platform, connected: connected, pairedAtUnix: 0,
                remoteControlAllowed: false, unlockAllowed: false
            )
        }
        XCTAssertTrue(status.canControl(device(.macOS)))
        XCTAssertTrue(status.canControl(device(.windows)))
        XCTAssertTrue(status.canControl(device(.linux)))
        XCTAssertFalse(status.canControl(device(.linux, connected: false)), "not connected")
        XCTAssertFalse(status.canControl(device(.android)), "phones can't be controlled")
        XCTAssertFalse(status.canControl(device(.iOS)))
        XCTAssertFalse(status.canControl(device(.unknown)))
        XCTAssertFalse(status.canControl(device(nil)), "not seen since Continuity started")
        status.controllingDevice = "office"
        XCTAssertFalse(status.canControl(device(.macOS)), "already controlling a computer")
        status.controllingDevice = nil
        status.canControlOthers = false
        XCTAssertFalse(status.canControl(device(.macOS)), "a build without the viewer")
    }

    func testAnUnknownPlatformDoesNotBreakDecoding() throws {
        var json = try XCTUnwrap(String(data: fixture(), encoding: .utf8))
        json = json.replacingOccurrences(of: "\"platform\": \"android\"", with: "\"platform\": \"fridge_os\"")
        let status = try XCTUnwrap(Envelope.decode(Data(json.utf8)).status)
        XCTAssertEqual(status.devices[0].platform, .unknown)
    }

    func testSendsTheTokenAndReturnsTheAnswer() async throws {
        let server = try FakeContinuityd(token: "secret") { request in
            XCTAssertEqual(request["token"] as? String, "secret")
            XCTAssertEqual(request["op"] as? String, "set_paused")
            XCTAssertEqual(request["paused"] as? Bool, true)
            return [#"{"ok":true,"message":""}"#]
        }
        let response = try await server.client.send(["op": "set_paused", "paused": true])
        XCTAssertTrue(response.ok)
        server.stop()
    }

    func testARefusedChangeComesBackWithItsMessage() async throws {
        let server = try FakeContinuityd(token: "secret") { _ in [#"{"ok":false,"message":"That device isn't paired."}"#] }
        let response = try await server.client.send(["op": "forget", "device": "nobody"])
        XCTAssertFalse(response.ok)
        XCTAssertEqual(response.message, "That device isn't paired.")
        server.stop()
    }

    func testWatchDeliversEveryStatusThenEnds() throws {
        let first = try fixture()
        let second = Data(String(decoding: first, as: UTF8.self).replacingOccurrences(of: "\"paused\": false", with: "\"paused\": true").utf8)
        let lines = [first, second].map { String(decoding: $0, as: UTF8.self).replacingOccurrences(of: "\n", with: "") }
        let server = try FakeContinuityd(token: "secret") { request in
            XCTAssertEqual(request["op"] as? String, "watch")
            return lines
        }
        let received = expectation(description: "two statuses")
        received.expectedFulfillmentCount = 2
        let ended = expectation(description: "the watch ends")
        var paused: [Bool] = []
        let watch = server.client.watch(
            onStatus: { status in
                paused.append(status.paused)
                received.fulfill()
            },
            onEnd: { _ in ended.fulfill() }
        )
        XCTAssertNotNil(watch)
        wait(for: [received, ended], timeout: 5)
        XCTAssertEqual(paused, [false, true])
        server.stop()
    }

    /// The window's new requests, as continuityd's `ControlOp` reads them
    /// (`requests_parse_by_op` in core/continuityd/src/control.rs).
    @MainActor
    func testTransfersFoldersAndRemoteControlRequests() async throws {
        let arrived = expectation(description: "six requests")
        arrived.expectedFulfillmentCount = 6
        var requests: [String: [String: Any]] = [:]
        let server = try FakeContinuityd(token: "secret") { request in
            XCTAssertEqual(request["token"] as? String, "secret")
            if let op = request["op"] as? String {
                requests[op] = request
            }
            arrived.fulfill()
            return [#"{"ok":true,"message":""}"#]
        }
        let model = SettingsModel(client: server.client)
        let office = PairedDevice(
            id: "office", name: "Office PC", platform: .windows, connected: true, pairedAtUnix: 0,
            remoteControlAllowed: false, unlockAllowed: false
        )
        let photo = Transfer(
            id: "t2", direction: .receiving, fileName: "IMG_2041.jpg", peerName: "Pixel 8", sizeBytes: 820_000, bytesDone: 820_000,
            state: .done, reason: nil, path: "/Users/me/Downloads/Continuity/IMG_2041.jpg", startedAtUnix: 0, finishedAtUnix: 1
        )
        model.requestRemoteControl(office)
        model.setReceivedFilesDir("/Users/me/Incoming")
        model.openReceivedFilesDir()
        model.openTransfer(photo, reveal: true)
        model.refreshNearby()
        model.cancelTransfer(photo)
        await fulfillment(of: [arrived], timeout: 5)
        server.stop()

        XCTAssertEqual(requests["request_remote_control"]?["device"] as? String, "office")
        XCTAssertEqual(requests["set_received_files_dir"]?["path"] as? String, "/Users/me/Incoming")
        XCTAssertEqual(requests["open_received_files_dir"]?.keys.sorted(), ["op", "token"])
        XCTAssertEqual(requests["open_transfer"]?["id"] as? String, "t2")
        XCTAssertEqual(requests["open_transfer"]?["reveal"] as? Bool, true)
        XCTAssertEqual(requests["refresh_nearby"]?.keys.sorted(), ["op", "token"])
        XCTAssertEqual(requests["cancel_transfer"]?["id"] as? String, "t2")
    }

    func testNotRunningWhenThereIsNoEndpoint() async {
        let client = ControlClient(endpointURL: URL(fileURLWithPath: "/nonexistent/share-endpoint.json"))
        do {
            _ = try await client.send(["op": "status"])
            XCTFail("expected an error")
        } catch {
            XCTAssertEqual(error.localizedDescription, ControlError.notRunning.localizedDescription)
        }
        XCTAssertNil(client.watch(onStatus: { _ in }, onEnd: { _ in }))
    }
}

/// A stand-in for continuityd's control socket: answers each connection's
/// one request line with the given lines, then closes it.
private final class FakeContinuityd {
    let client: ControlClient
    private let listener: NWListener
    private let endpointFile: URL

    init(token: String, respond: @escaping ([String: Any]) -> [String]) throws {
        listener = try NWListener(using: .tcp, on: .any)
        let queue = DispatchQueue(label: "fake-continuityd")
        listener.newConnectionHandler = { connection in
            connection.start(queue: queue)
            connection.receive(minimumIncompleteLength: 1, maximumLength: 64 * 1024) { data, _, _, _ in
                let line = data.flatMap { String(data: $0, encoding: .utf8) }?.split(separator: "\n").first.map(String.init) ?? ""
                let request = (try? JSONSerialization.jsonObject(with: Data(line.utf8))) as? [String: Any] ?? [:]
                let reply = respond(request).map { $0 + "\n" }.joined()
                connection.send(content: Data(reply.utf8), completion: .contentProcessed { _ in
                    connection.cancel()
                })
            }
        }
        let ready = DispatchSemaphore(value: 0)
        listener.stateUpdateHandler = { state in
            if case .ready = state { ready.signal() }
        }
        listener.start(queue: queue)
        guard ready.wait(timeout: .now() + 5) == .success, let port = listener.port?.rawValue else {
            throw ControlError.notRunning
        }
        endpointFile = FileManager.default.temporaryDirectory.appendingPathComponent("share-endpoint-\(UUID().uuidString).json")
        try Data(#"{"port":\#(port),"token":"\#(token)"}"#.utf8).write(to: endpointFile)
        client = ControlClient(endpointURL: endpointFile)
    }

    func stop() {
        listener.cancel()
        try? FileManager.default.removeItem(at: endpointFile)
    }
}
