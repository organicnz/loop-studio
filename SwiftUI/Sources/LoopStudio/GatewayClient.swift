import Foundation

// MARK: - Gateway Client
//
// The one Swift implementation of the gateway's OpenAI-compatible protocol.
// Chat, Code, and Loops each keep their own state machine and prompt policy;
// URL construction, auth, request encoding, SSE decoding, error-body draining,
// and cancellation live here.
//
// The interface is deliberately small: a semantic request in, typed events out,
// and one error type. `GatewayTransport` is the seam below it, so the protocol
// can be tested without a router or a socket.

struct GatewayMessage: Encodable, Sendable {
    var role: String
    var content: String
}

struct GatewayRequest: Sendable {
    var model: String
    var messages: [GatewayMessage]
    var temperature: Double?
    var topP: Double?
    var maxTokens: Int?
    var stream: Bool
    var timeout: TimeInterval

    init(
        model: String,
        messages: [GatewayMessage],
        temperature: Double? = nil,
        topP: Double? = nil,
        maxTokens: Int? = nil,
        stream: Bool = true,
        timeout: TimeInterval = 300
    ) {
        self.model = model
        self.messages = messages
        self.temperature = temperature
        self.topP = topP
        self.maxTokens = maxTokens
        self.stream = stream
        self.timeout = timeout
    }
}

enum GatewayEvent: Sendable, Equatable {
    case content(String)
    case reasoning(String)
}

struct GatewayReply: Sendable, Equatable {
    var content: String
    var reasoning: String
}

enum GatewayError: LocalizedError, Sendable, Equatable {
    case badURL
    case transport(String)
    case http(Int, String)
    case malformedResponse(String)

    var errorDescription: String? {
        switch self {
        case .badURL:
            return "Invalid gateway URL."
        case .transport(let what):
            return "Request failed: \(what)"
        case .http(let code, let detail):
            if code == 401 {
                return "401 Unauthorized — set the gateway token (Tailscale remote).\(detail.isEmpty ? "" : " \(detail)")"
            }
            return "Gateway returned HTTP \(code)\(detail.isEmpty ? "." : ": \(detail)")"
        case .malformedResponse(let body):
            return "Gateway returned no assistant message.\(body.isEmpty ? "" : " \(body)")"
        }
    }
}

@MainActor
struct GatewayClient {
    private let connection: LoopStudioConnectionStore
    private let transport: any GatewayTransport

    init(
        connection: LoopStudioConnectionStore = .shared,
        transport: any GatewayTransport = URLSessionGatewayTransport()
    ) {
        self.connection = connection
        self.transport = transport
    }

    /// Stream one completion, delivering typed pieces as they arrive.
    ///
    /// Cancellation is rethrown as `CancellationError`, never wrapped into a
    /// transport error: the stores distinguish "the user pressed Stop" from
    /// "the gateway failed", and that distinction is the whole reason a
    /// cancelled stream still keeps its partial text.
    func stream(
        _ request: GatewayRequest,
        onEvent: @MainActor @Sendable (GatewayEvent) -> Void
    ) async throws -> GatewayReply {
        var request = request
        request.stream = true
        let urlRequest = try makeURLRequest(request)
        let response = try await send(urlRequest)
        guard (200...299).contains(response.statusCode) else {
            throw GatewayError.http(response.statusCode, await drainError(from: response.lines))
        }

        var content = ""
        var reasoning = ""
        let decoder = JSONDecoder()

        do {
            try await withTaskCancellationHandler {
                for try await line in response.lines {
                    try Task.checkCancellation()
                    let trimmed = line.trimmingCharacters(in: .whitespaces)
                    guard trimmed.hasPrefix("data:") else { continue }
                    let payload = String(trimmed.dropFirst(5))
                        .trimmingCharacters(in: .whitespaces)
                    if payload == "[DONE]" { break }
                    guard let data = payload.data(using: .utf8),
                          let chunk = try? decoder.decode(StreamChunk.self, from: data) else {
                        continue
                    }
                    for choice in chunk.choices ?? [] {
                        if let piece = choice.delta?.reasoning_content, !piece.isEmpty {
                            reasoning += piece
                            onEvent(.reasoning(piece))
                        }
                        if let piece = choice.delta?.content, !piece.isEmpty {
                            content += piece
                            onEvent(.content(piece))
                        }
                    }
                }
            } onCancel: {
                response.cancel()
            }
        } catch let error as URLError where error.code == .cancelled {
            throw CancellationError()
        }

        return GatewayReply(content: content, reasoning: reasoning)
    }

    /// One non-streaming completion. Used where the caller needs the reply,
    /// not the arrival: Loops' judgment and implement turns.
    func complete(_ request: GatewayRequest) async throws -> String {
        var request = request
        request.stream = false
        let urlRequest = try makeURLRequest(request)
        let response = try await send(urlRequest)
        guard (200...299).contains(response.statusCode) else {
            throw GatewayError.http(response.statusCode, await drainError(from: response.lines))
        }

        var body = ""
        do {
            try await withTaskCancellationHandler {
                for try await line in response.lines {
                    try Task.checkCancellation()
                    body += line
                    body += "\n"
                }
            } onCancel: {
                response.cancel()
            }
        } catch let error as URLError where error.code == .cancelled {
            throw CancellationError()
        }

        // A 200 without a message is not a reply. The previous Loops client
        // returned the raw body instead, which could hand a proxy error page
        // to the reviewer as if the model had produced it.
        guard let content = Self.completionContent(from: body) else {
            throw GatewayError.malformedResponse(String(body.prefix(300)))
        }
        return content
    }

    // MARK: - Request construction

    private func makeURLRequest(_ request: GatewayRequest) throws -> URLRequest {
        guard let url = connection.url(path: "/v1/chat/completions") else {
            throw GatewayError.badURL
        }
        var req = URLRequest(url: url)
        req.httpMethod = "POST"
        req.setValue("application/json", forHTTPHeaderField: "Content-Type")
        connection.authorize(&req)
        req.timeoutInterval = request.timeout
        do {
            req.httpBody = try JSONEncoder().encode(WireRequest(request))
        } catch {
            throw GatewayError.transport("could not encode the request: \(error.localizedDescription)")
        }
        return req
    }

    private func send(_ request: URLRequest) async throws -> GatewayResponse {
        do {
            return try await transport.send(request)
        } catch is CancellationError {
            throw CancellationError()
        } catch let error as URLError where error.code == .cancelled {
            throw CancellationError()
        } catch let error as GatewayError {
            throw error
        } catch {
            throw GatewayError.transport(error.localizedDescription)
        }
    }

    // MARK: - Error bodies

    /// Drain a short detail from a failed response.
    ///
    /// The router's failure shape varies: a JSON body, an SSE `data:` line,
    /// or plain text from whatever is in front of it. Twenty lines is enough
    /// to find a message and small enough that a broken gateway cannot make
    /// an error path read forever.
    private func drainError(from lines: AsyncThrowingStream<String, Error>) async -> String {
        var detail = ""
        var seen = 0
        do {
            for try await line in lines {
                let trimmed = line.trimmingCharacters(in: .whitespaces)
                if trimmed.hasPrefix("data:") {
                    let payload = String(trimmed.dropFirst(5))
                        .trimmingCharacters(in: .whitespaces)
                    if let message = Self.errorMessage(from: payload) {
                        detail = message
                        break
                    }
                } else if let message = Self.errorMessage(from: trimmed) {
                    detail = message
                    break
                } else if !trimmed.isEmpty && detail.isEmpty {
                    detail = trimmed
                }
                seen += 1
                if seen >= 20 { break }
            }
        } catch {
            // Keep whatever arrived. The status code is the primary error.
        }
        return String(detail.prefix(300))
    }

    private static func errorMessage(from text: String) -> String? {
        guard let data = text.data(using: .utf8),
              let envelope = try? JSONDecoder().decode(ErrorEnvelope.self, from: data),
              let message = envelope.error?.message?
                .trimmingCharacters(in: .whitespacesAndNewlines),
              !message.isEmpty else {
            return nil
        }
        return String(message.prefix(300))
    }

    private static func completionContent(from body: String) -> String? {
        guard let data = body.data(using: .utf8),
              let envelope = try? JSONDecoder().decode(CompletionEnvelope.self, from: data),
              let content = envelope.choices?.first?.message?.content else {
            return nil
        }
        return content
    }

    // MARK: - Wire types

    private struct WireRequest: Encodable {
        let model: String
        let messages: [GatewayMessage]
        let stream: Bool
        let temperature: Double?
        let top_p: Double?
        let max_tokens: Int?

        init(_ request: GatewayRequest) {
            model = request.model
            messages = request.messages
            stream = request.stream
            temperature = request.temperature
            top_p = request.topP
            max_tokens = request.maxTokens
        }
    }

    private struct StreamChunk: Decodable {
        var choices: [StreamChoice]?
    }

    private struct StreamChoice: Decodable {
        var delta: StreamDelta?
    }

    private struct StreamDelta: Decodable {
        var content: String?
        var reasoning_content: String?
    }

    private struct ErrorEnvelope: Decodable {
        struct Body: Decodable {
            var message: String?
        }
        var error: Body?
    }

    private struct CompletionEnvelope: Decodable {
        struct Choice: Decodable {
            struct Message: Decodable {
                var content: String?
            }
            var message: Message?
        }
        var choices: [Choice]?
    }
}
