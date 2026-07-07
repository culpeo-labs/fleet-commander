---
cargo/fleet-commander: minor
---

Make the host's `fleet-agent` transport serve **concurrent requests** (Phase 4b2 y3, part 2).

Previously `ProcessTransport.call` was strictly serial: a single locked call channel meant "the next response on the wire is mine", so a slow request (e.g. a `session.start` handshake) head-of-line-blocked every other call on the same connection. The transport now multiplexes: each in-flight request registers a per-id waiter before it writes, the reader thread routes each response to its matching waiter by id, and writes only briefly hold the stdin lock (released before awaiting the reply). On EOF or a protocol error the reader drains all pending waiters with a broken-pipe error so no call sits out its full deadline. This lets the explorer's `fs.*` traffic and a long-running `session.*` handshake share one connection without freezing each other — the prerequisite for collapsing the per-agent `fs`/`git`/`session` bridges onto a single `docker exec`.

The `ServiceFs` now also holds its transport behind an `Arc` so it can be shared with the session driver in the follow-up unification.
