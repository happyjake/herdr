# Recorded attachment.create wire fixture

`attachment-recording.json` is a capture of real traffic between a raw
WebSocket client and this fork's WS listener, recorded for the sample-notes
repo's fake herdr server — one contract, two consumers, the same discipline
as that repo's pane-read recordings. The envelope matches:
`{firstLiveFrameIndex, recording: [{dir: "send"|"recv", ms, frame}]}` with a
monotonic tick as `ms` and `firstLiveFrameIndex: 0` (no ring replay is
involved; every frame is live).

Choreography:

1. `ping` → `pong`.
2. `attachment.create` with sniff-valid PNG bytes → `attachment_created`
   with the absolute, space-free server-generated path (`.png`, the
   extension the magic bytes dictate) and `expires_at` (unix seconds,
   creation + 24 h TTL).
3. The same with JPEG bytes → a `.jpg` path.
4. Unrecognized bytes → error `attachment_unsupported_format`.
5. Malformed base64 → error `invalid_params`.

The image payloads are minimal magic-byte-valid byte strings, not decodable
pictures — the contract under test is the JSON exchange and the path/expiry
semantics, which is all the fake server replays. `attachment_too_large`
does not appear here because a payload past the decoded cap cannot fit the
unchanged 1 MiB per-message transport cap; the code exists as defense in
depth and is pinned by unit tests in the fork.

To re-record against a live server (paths and expiries will change; the
validating test only checks contract shape):

```bash
HERDR_UPDATE_ATTACHMENT_FIXTURE=1 just test-one attachment_fixture
```

The recorder and the shape validation live in `tests/ws_api.rs`
(`attachment_fixture_for_the_mobile_fake_server_is_current`).
