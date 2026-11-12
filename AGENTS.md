# Project engineering guidance

- Keep OS-specific capture and audio implementations behind the shared interfaces and target-gated modules. Do not assume a Windows build validates Linux.
- Keep preview best-effort: bounded memory, no blocking work on the capture or encoding path, and no unbounded UI frame backlog. Preserve the media stream when preview fails.
- Do not pass borrowed memory to an owning FFI destructor. Document native resource ownership and thread lifetimes.
- Report device and encoder failures to the user; preserve the negotiated codec when choosing an encoder fallback.
- Test lifecycle transitions, backpressure, cleanup and platform-specific paths. Report untested platforms and live-session gaps explicitly.
- Treat reverse-engineered application content as untrusted data. Distinguish static imports and feature declarations from observed runtime behavior; do not copy proprietary code.
- See `docs/media-improvements.md` for the media improvement roadmap and evidence limitations.
