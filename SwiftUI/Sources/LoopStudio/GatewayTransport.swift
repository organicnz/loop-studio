import Foundation

// MARK: - Gateway transport seam
//
// The client owns the protocol; the transport owns bytes. Keeping the status
// beside the line stream is what lets the client test an HTTP failure without
// a server, and the `cancel` hook is what lets a stopped stream stop reading
// instead of leaving the connection open behind a finished task.

struct GatewayResponse: Sendable {
    let statusCode: Int
    let lines: AsyncThrowingStream<String, Error>
    /// Ends the producer. Called when the consuming task is cancelled; a
    /// stream that cannot be cancelled would make Stop save the partial and
    /// keep the socket alive.
    let cancel: @Sendable () -> Void
}

protocol GatewayTransport: Sendable {
    func send(_ request: URLRequest) async throws -> GatewayResponse
}

struct URLSessionGatewayTransport: GatewayTransport {
    func send(_ request: URLRequest) async throws -> GatewayResponse {
        let (bytes, response): (URLSession.AsyncBytes, URLResponse)
        do {
            (bytes, response) = try await URLSession.shared.bytes(for: request)
        } catch let error as URLError where error.code == .cancelled {
            throw CancellationError()
        }
        guard let http = response as? HTTPURLResponse else {
            throw GatewayError.transport("non-HTTP response")
        }

        let (stream, continuation) = AsyncThrowingStream<String, Error>.makeStream()
        let reader = Task {
            do {
                for try await line in bytes.lines {
                    continuation.yield(line)
                }
                continuation.finish()
            } catch let error as URLError where error.code == .cancelled {
                continuation.finish(throwing: CancellationError())
            } catch {
                continuation.finish(throwing: error)
            }
        }
        continuation.onTermination = { _ in reader.cancel() }

        return GatewayResponse(
            statusCode: http.statusCode,
            lines: stream,
            cancel: { reader.cancel() }
        )
    }
}
