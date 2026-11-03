import Foundation
import Testing

@testable import LoopStudio

// MARK: - Test doubles

private final class RequestRecorder: @unchecked Sendable {
    private let lock = NSLock()
    private var stored: URLRequest?

    func record(_ request: URLRequest) {
        lock.lock()
        stored = request
        lock.unlock()
    }

    var request: URLRequest? {
        lock.lock()
        defer { lock.unlock() }
        return stored
    }
}

private struct ScriptedTransport: GatewayTransport {
    let statusCode: Int
    let lines: [String]
    let recorder: RequestRecorder?
    let holdsStreamOpen: Bool

    init(
        statusCode: Int,
        lines: [String],
        recorder: RequestRecorder? = nil,
        holdsStreamOpen: Bool = false
    ) {
        self.statusCode = statusCode
        self.lines = lines
        self.recorder = recorder
        self.holdsStreamOpen = holdsStreamOpen
    }

    func send(_ request: URLRequest) async throws -> GatewayResponse {
        recorder?.record(request)
        let (stream, continuation) = AsyncThrowingStream<String, Error>.makeStream()
        for line in lines {
            continuation.yield(line)
        }
        if !holdsStreamOpen {
            continuation.finish()
        }
        return GatewayResponse(
            statusCode: statusCode,
            lines: stream,
            cancel: { continuation.finish(throwing: CancellationError()) }
        )
    }
}

@MainActor
private final class EventLog {
    var events: [GatewayEvent] = []
}

@MainActor
private func makeClient(
    _ transport: any GatewayTransport,
    token: String = ""
) -> GatewayClient {
    let connection = LoopStudioConnectionStore(
        host: "127.0.0.1",
        port: 8000,
        useTLS: false,
        token: token
    )
    return GatewayClient(connection: connection, transport: transport)
}

private func makeRequest(stream: Bool = true) -> GatewayRequest {
    GatewayRequest(
        model: "qwen3.8-27b",
        messages: [
            GatewayMessage(role: "system", content: "be brief"),
            GatewayMessage(role: "user", content: "hello"),
        ],
        temperature: 0.6,
        topP: 0.95,
        maxTokens: 4096,
        stream: stream,
        timeout: 20
    )
}

private func temporaryDirectory() -> URL {
    let url = FileManager.default.temporaryDirectory
        .appendingPathComponent("loop-studio-gateway-tests-\(UUID().uuidString)")
    try? FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
    return url
}

// MARK: - Streaming

struct GatewayStreamTests {
    @Test @MainActor func reasoningAndContentArriveAsTypedEvents() async throws {
        let transport = ScriptedTransport(statusCode: 200, lines: [
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"plan\"}}]}",
            "",
            "data: {\"choices\":[{\"delta\":{\"content\":\"answer\"}}]}",
            "data: [DONE]",
            "data: {\"choices\":[{\"delta\":{\"content\":\"after done\"}}]}",
        ])
        let log = EventLog()

        let reply = try await makeClient(transport).stream(makeRequest()) { event in
            log.events.append(event)
        }

        #expect(log.events == [.reasoning("plan"), .content("answer")])
        #expect(reply == GatewayReply(content: "answer", reasoning: "plan"))
    }

    @Test @MainActor func nonDataLinesAreIgnored() async throws {
        let transport = ScriptedTransport(statusCode: 200, lines: [
            ": keep-alive",
            "",
            "event: message",
            "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}",
            "data: [DONE]",
        ])
        let log = EventLog()

        let reply = try await makeClient(transport).stream(makeRequest()) { event in
            log.events.append(event)
        }

        #expect(log.events == [.content("ok")])
        #expect(reply.content == "ok")
    }

    @Test @MainActor func multipleChoicesAreAllDelivered() async throws {
        let transport = ScriptedTransport(statusCode: 200, lines: [
            "data: {\"choices\":[{\"delta\":{\"content\":\"a\"}},{\"delta\":{\"content\":\"b\"}}]}",
            "data: [DONE]",
        ])

        let reply = try await makeClient(transport).stream(makeRequest()) { _ in }

        #expect(reply.content == "ab")
    }
}

// MARK: - Errors

struct GatewayErrorTests {
    @Test @MainActor func jsonErrorBodyBecomesOneGatewayError() async throws {
        let transport = ScriptedTransport(statusCode: 500, lines: [
            "{\"error\":{\"message\":\"internal error\"}}",
        ])

        do {
            _ = try await makeClient(transport).complete(makeRequest(stream: false))
            Issue.record("expected an HTTP error")
        } catch let error as GatewayError {
            #expect(error == .http(500, "internal error"))
        }
    }

    @Test @MainActor func sseErrorBodyBecomesOneGatewayError() async throws {
        let transport = ScriptedTransport(statusCode: 503, lines: [
            "data: {\"error\":{\"message\":\"no backend\"}}",
        ])

        do {
            _ = try await makeClient(transport).stream(makeRequest()) { _ in }
            Issue.record("expected an HTTP error")
        } catch let error as GatewayError {
            #expect(error == .http(503, "no backend"))
        }
    }

    @Test @MainActor func errorDetailIsBounded() async throws {
        let transport = ScriptedTransport(
            statusCode: 500,
            lines: Array(repeating: String(repeating: "x", count: 400), count: 30)
        )

        do {
            _ = try await makeClient(transport).stream(makeRequest()) { _ in }
            Issue.record("expected an HTTP error")
        } catch let error as GatewayError {
            guard case .http(500, let detail) = error else {
                Issue.record("wrong error: \(error)")
                return
            }
            #expect(detail.count == 300)
            #expect(!detail.isEmpty)
        }
    }

    @Test func unauthorizedExplainsTheToken() {
        let error = GatewayError.http(401, "")
        #expect(error.errorDescription?.contains("set the gateway token") == true)
    }

    @Test func badURLCarriesItsOwnCase() {
        #expect(GatewayError.badURL.errorDescription == "Invalid gateway URL.")
    }
}

// MARK: - Requests and completions

struct GatewayRequestTests {
    @Test @MainActor func requestCarriesAuthTimeoutAndOmitsUnsetFields() async throws {
        let recorder = RequestRecorder()
        let transport = ScriptedTransport(
            statusCode: 200,
            lines: ["data: [DONE]"],
            recorder: recorder
        )
        var request = makeRequest()
        request.maxTokens = nil

        _ = try await makeClient(transport, token: "secret").stream(request) { _ in }

        let recorded = try #require(recorder.request)
        #expect(recorded.value(forHTTPHeaderField: "Authorization") == "Bearer secret")
        #expect(recorded.timeoutInterval == 20)
        #expect(recorded.url?.path == "/v1/chat/completions")
        #expect(recorded.httpMethod == "POST")

        let body = try #require(recorded.httpBody)
        let object = try JSONSerialization.jsonObject(with: body)
        let json = try #require(object as? [String: Any])
        #expect(json["model"] as? String == "qwen3.8-27b")
        #expect(json["stream"] as? Bool == true)
        #expect(json["max_tokens"] == nil)
        #expect(json["top_p"] as? Double == 0.95)
    }

    @Test @MainActor func completeSendsANonStreamingRequestAndDecodesTheMessage() async throws {
        let recorder = RequestRecorder()
        let transport = ScriptedTransport(
            statusCode: 200,
            lines: ["{\"choices\":[{\"message\":{\"content\":\"done\"}}]}"],
            recorder: recorder
        )

        let reply = try await makeClient(transport).complete(makeRequest(stream: true))

        #expect(reply == "done")
        let recorded = try #require(recorder.request)
        let body = try #require(recorded.httpBody)
        let object = try JSONSerialization.jsonObject(with: body)
        let json = try #require(object as? [String: Any])
        #expect(json["stream"] as? Bool == false)
    }

    @Test @MainActor func aTwoHundredWithoutAMessageIsNotAReply() async throws {
        let transport = ScriptedTransport(statusCode: 200, lines: ["{\"oops\":true}"])

        do {
            _ = try await makeClient(transport).complete(makeRequest(stream: false))
            Issue.record("expected a malformed-response error")
        } catch let error as GatewayError {
            guard case .malformedResponse(let body) = error else {
                Issue.record("wrong error: \(error)")
                return
            }
            #expect(body.contains("oops"))
        }
    }
}

// MARK: - Cancellation and Chat's rendering policy

struct GatewayCancellationTests {
    @Test @MainActor func cancellationIsNotATransportError() async throws {
        let transport = ScriptedTransport(
            statusCode: 200,
            lines: ["data: {\"choices\":[{\"delta\":{\"content\":\"half\"}}]}"],
            holdsStreamOpen: true
        )
        let client = makeClient(transport)

        let task = Task { @MainActor in
            try await client.stream(makeRequest()) { _ in }
        }
        try await Task.sleep(nanoseconds: 50_000_000)
        task.cancel()

        do {
            _ = try await task.value
            Issue.record("expected cancellation")
        } catch is CancellationError {
            // This is the distinction the stores rely on.
        } catch {
            Issue.record("cancellation became \(error)")
        }
    }

    @Test @MainActor func chatOpensAndClosesTheThinkingTag() {
        let root = temporaryDirectory()
        defer { try? FileManager.default.removeItem(at: root) }
        let store = ChatStore(rootURL: root)

        store.applyStreamEvent(.reasoning("plan"))
        store.applyStreamEvent(.content("answer"))
        store.finishThinking()

        #expect(store.streamText == " thinking\nplan\n</think>\nanswer")
    }

    @Test @MainActor func chatClosesAnOpenThinkingTagAtTheEnd() {
        let root = temporaryDirectory()
        defer { try? FileManager.default.removeItem(at: root) }
        let store = ChatStore(rootURL: root)

        store.applyStreamEvent(.reasoning("plan"))
        store.finishThinking()

        #expect(store.streamText == " thinking\nplan\n</think>\n")
    }

    @Test func gatewayHTTPErrorsMapToChatsTestedWording() {
        let mapped = ChatStore.mapGatewayError(GatewayError.http(502, ""))
        #expect((mapped as? ChatError)?.errorDescription?.contains("No backend served") == true)

        let transport = ChatStore.mapGatewayError(GatewayError.transport("boom"))
        #expect((transport as? ChatError)?.errorDescription == "Request failed: boom")
    }
}
