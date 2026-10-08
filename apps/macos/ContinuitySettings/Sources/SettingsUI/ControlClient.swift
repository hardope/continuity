import Foundation
import Network

/// Why talking to `continuityd` didn't work, worded for the window.
public enum ControlError: LocalizedError {
    case notRunning
    case connectionLost
    case badResponse

    public var errorDescription: String? {
        switch self {
        case .notRunning: "Continuity isn't running."
        case .connectionLost: "Lost the connection to Continuity."
        case .badResponse: "Continuity sent something this window doesn't understand."
        }
    }
}

/// Where the running `continuityd` listens, and the token every request
/// has to carry — published in a file only this user can read
/// (`share-endpoint.<profile>.json`, next to the trust store).
struct Endpoint: Decodable {
    var port: UInt16
    var token: String
}

/// Talks to `continuityd`'s local control socket: one JSON request line
/// per connection, JSON lines back (see core/continuityd/src/control.rs).
final class ControlClient {
    private let endpointURL: URL

    init(profile: String) {
        // `directories::ProjectDirs::from("app", "continuity", "continuity")`'s
        // config directory on macOS, where continuityd writes it.
        endpointURL = FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent("Library/Application Support/app.continuity.continuity", isDirectory: true)
            .appendingPathComponent("share-endpoint.\(profile).json")
    }

    /// Reads the endpoint from somewhere else — for tests.
    init(endpointURL: URL) {
        self.endpointURL = endpointURL
    }

    private func endpoint() throws -> Endpoint {
        guard let data = try? Data(contentsOf: endpointURL),
              let endpoint = try? JSONDecoder().decode(Endpoint.self, from: data)
        else { throw ControlError.notRunning }
        return endpoint
    }

    private func requestLine(_ fields: [String: Any], token: String) throws -> Data {
        var fields = fields
        fields["token"] = token
        var line = try JSONSerialization.data(withJSONObject: fields)
        line.append(UInt8(ascii: "\n"))
        return line
    }

    /// Sends one request and returns its one response.
    func send(_ fields: [String: Any]) async throws -> Envelope {
        let endpoint = try endpoint()
        let line = try requestLine(fields, token: endpoint.token)
        return try await withCheckedThrowingContinuation { continuation in
            let connection = LineConnection(port: endpoint.port)
            var answered = false
            connection.open(
                request: line,
                onLine: { data in
                    guard !answered else { return }
                    answered = true
                    connection.close()
                    do {
                        continuation.resume(returning: try Envelope.decode(data))
                    } catch {
                        continuation.resume(throwing: ControlError.badResponse)
                    }
                },
                onClose: { _ in
                    guard !answered else { return }
                    answered = true
                    continuation.resume(throwing: ControlError.notRunning)
                }
            )
        }
    }

    /// Streams status snapshots until the connection ends; `onEnd` runs
    /// once, at the end. `nil` if continuityd isn't running at all.
    func watch(onStatus: @escaping (Status) -> Void, onEnd: @escaping (Error?) -> Void) -> LineConnection? {
        guard let endpoint = try? endpoint(), let line = try? requestLine(["op": "watch"], token: endpoint.token) else {
            return nil
        }
        let connection = LineConnection(port: endpoint.port)
        connection.open(
            request: line,
            onLine: { data in
                if let status = try? Envelope.decode(data).status {
                    onStatus(status)
                }
            },
            onClose: onEnd
        )
        return connection
    }
}

/// One TCP connection to the loopback control socket: sends a request,
/// then hands over every newline-terminated line that comes back. All
/// callbacks run on one private serial queue.
final class LineConnection {
    private let connection: NWConnection
    private let queue = DispatchQueue(label: "app.continuity.settings.connection")
    private var buffer = Data()
    private var onLine: ((Data) -> Void)?
    private var onClose: ((Error?) -> Void)?

    init(port: UInt16) {
        connection = NWConnection(host: .ipv4(.loopback), port: NWEndpoint.Port(rawValue: port) ?? .any, using: .tcp)
    }

    deinit {
        connection.cancel()
    }

    func open(request: Data, onLine: @escaping (Data) -> Void, onClose: @escaping (Error?) -> Void) {
        self.onLine = onLine
        self.onClose = onClose
        connection.stateUpdateHandler = { [weak self] state in
            switch state {
            case .ready:
                self?.send(request)
            case .waiting(let error), .failed(let error):
                // A refused connection only ever "waits" for a retry —
                // nothing is listening there any more.
                self?.finish(error)
            case .cancelled:
                self?.finish(nil)
            default:
                break
            }
        }
        connection.start(queue: queue)
    }

    func close() {
        connection.cancel()
    }

    private func send(_ request: Data) {
        connection.send(content: request, completion: .contentProcessed { [weak self] error in
            if let error {
                self?.finish(error)
            } else {
                self?.receive()
            }
        })
    }

    private func receive() {
        connection.receive(minimumIncompleteLength: 1, maximumLength: 256 * 1024) { [weak self] data, _, isComplete, error in
            guard let self else { return }
            if let data {
                self.buffer.append(data)
                self.deliverLines()
            }
            if let error {
                self.finish(error)
            } else if isComplete {
                self.finish(nil)
            } else {
                self.receive()
            }
        }
    }

    private func deliverLines() {
        while let newline = buffer.firstIndex(of: UInt8(ascii: "\n")) {
            let line = buffer.subdata(in: buffer.startIndex..<newline)
            buffer.removeSubrange(buffer.startIndex...newline)
            onLine?(line)
        }
    }

    private func finish(_ error: Error?) {
        let onClose = self.onClose
        self.onClose = nil
        self.onLine = nil
        connection.cancel()
        onClose?(error)
    }
}
