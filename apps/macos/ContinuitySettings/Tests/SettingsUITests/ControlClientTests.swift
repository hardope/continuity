import Foundation
import Network
import XCTest
@testable import SettingsUI

/// The settings window's side of the control protocol, against a fake
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
