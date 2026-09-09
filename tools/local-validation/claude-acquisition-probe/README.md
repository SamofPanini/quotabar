# Local Claude acquisition probe (P4B-1)

This is a temporary MV3 Chromium probe for the P4B acquisition gate. It is not part of QuotaBar, has no production dependency, and does not add a bridge, listener, persistence, or network service.

## Synthetic validation only

Live validation is forbidden until Sol High accepts the PR. Do not load this against an authenticated browser profile, send a Claude message, or capture provider traffic. The included tests use only synthetic, secret-free data.

After approval for P4B-2, load this directory as an unpacked extension only in the two separately authorized browser contexts. Assign only the synthetic ledger labels `profile-a` and `profile-b`. Never record actual browser profile names or paths.

## Install and remove

1. In Chromium, open `chrome://extensions`, enable Developer mode, and choose **Load unpacked**.
2. Select this directory. In the popup, select and confirm the synthetic slot and operator-declared plan class **before** any authorized trigger. They are never inferred from provider data.
3. Use separate browser profiles for the two authorized accounts; record only `profile-a` and `profile-b`, never profile names or paths. The popup displays only a sanitized envelope.
4. To remove it, open `chrome://extensions` and choose **Remove**. Selection and sanitized observations live only in `chrome.storage.session`: they survive service-worker restarts but clear when the browser session ends.

## Safety boundary

The page-world observer recognizes only exact same-origin GET usage responses and POST completion/retry-completion SSE responses with `text/event-stream` content type. The opaque route segment stays in page world. It sends the content script only an allowlisted, validated envelope; the extension stores only that envelope in session storage, never credentials, raw data, or conversation content. It makes no network requests and does not modify, block, or delay Claude requests.

Authenticated loading remains forbidden pending Sol High re-review.

Run focused synthetic tests from the repository root:

```bash
npx vitest run tools/local-validation/claude-acquisition-probe/probe.test.js
```
