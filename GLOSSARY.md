# Glossary

Terms this project uses in code, docs, and reviews. Add a term here when a module is
named after it and the meaning is not plain from the name.

- **Gateway** — `loop-studio-router` on `:8000`; the one OpenAI-compatible entry point
  every client speaks to. It owns routing, lane health, and auth.
- **Lane** — one inference backend behind the gateway: MLX (`:8080`),
  llama-server (`:8081`), or Ollama (`:11434`).
- **Gateway client** — the one Swift module that speaks the gateway's
  OpenAI-compatible protocol. Its interface is a request in and streamed events out;
  URL construction, auth, SSE decoding, error-body draining, and cancellation are
  implementation behind the seam.
