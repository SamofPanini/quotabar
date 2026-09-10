# Local Claude acquisition probe (P4B-1)

This is a temporary MV3 Chromium probe for the P4B acquisition gate. It is not part of QuotaBar, has no production dependency, and does not add a bridge, listener, persistence, or network service.

## Synthetic validation and live authorization

The included tests use only synthetic, secret-free data. Live loading is allowed only when the control plane authorizes the exact reviewed head, and only in the separately authorized browser contexts. Assign only the synthetic ledger labels `profile-a` and `profile-b`; never record actual browser profile names or paths. P4B-2R and P4B-2S are not authorized by this implementation task.

Paid and Free both require a real completion/retry-completion SSE `message_limit.windows` observation as primary live acquisition evidence. Static Settings or same-origin `/usage` data is optional, provisional cross-check only: its absence is not failure, and its presence is never enough for a P4B live PASS. A live gate requires one separately owner-authorized ordinary message in each profile. A no-message run proves injection readiness only, not acquisition.

## Install and remove

1. In Chromium, open `chrome://extensions`, enable Developer mode, and choose **Load unpacked**.
2. Select this directory. In the popup, select and confirm the synthetic slot and operator-declared plan class **before** any authorized trigger. They are never inferred from provider data.
3. Use separate browser profiles for the two authorized accounts; record only `profile-a` and `profile-b`, never profile names or paths. The popup displays a fixed build label, fixed readiness/acquisition stage, and a sanitized envelope.
4. To remove it, open `chrome://extensions` and choose **Remove**. Selection and sanitized observations live only in `chrome.storage.session`: they survive service-worker restarts but clear when the browser session ends.

## Safety boundary

The page-world observer recognizes only exact same-origin GET usage responses and POST completion/retry-completion SSE responses with `text/event-stream` content type. The opaque route segment stays in page world. It uses a bounded scan of each cloned SSE stream only to locate `message_limit`; it never emits, persists, logs, or exfiltrates raw SSE or other event content. It sends the content script only a fixed-stage `postMessage` or an allowlisted, validated envelope. The extension stores only fixed diagnostic stages and sanitized envelopes in session storage, never credentials, raw data, route/account identifiers, provider error strings, assistant text, prompt text, thinking/tool data, cookies, tokens, or headers. It makes no network requests and does not modify, block, or delay Claude requests.

The fixed stages distinguish bridge load, MAIN observer installation, MAIN readiness received by the bridge, completion match, event-stream recognition, `message_limit` discovery, stream completion without `message_limit`, and sanitized observation storage. They contain no request, provider, identity, or arbitrary error data. Sol acceptance never itself authorizes authenticated loading or message sending.

The lugia19 projects are mechanism references only: no GPL code, assets, runtime dependency, or Firebase path is used.

Run focused synthetic tests from the repository root:

```bash
npx vitest run tools/local-validation/claude-acquisition-probe/probe.test.js
```
