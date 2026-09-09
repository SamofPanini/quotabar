# Local Claude acquisition probe (P4B-1)

This is a temporary MV3 Chromium probe for the P4B acquisition gate. It is not part of QuotaBar, has no production dependency, and does not add a bridge, listener, persistence, or network service.

## Synthetic validation only

Live validation is forbidden until Sol High accepts the PR. Do not load this against an authenticated browser profile, send a Claude message, or capture provider traffic. The included tests use only synthetic, secret-free data.

After approval for P4B-2, load this directory as an unpacked extension only in the two separately authorized browser contexts. Assign only the synthetic ledger labels `profile-a` and `profile-b`. Never record actual browser profile names or paths.

## Install and remove

1. In Chromium, open `chrome://extensions`, enable Developer mode, and choose **Load unpacked**.
2. Select this directory. The popup can select only `profile-a` or `profile-b`; set the plan class as an explicit local operator declaration (it is never inferred from provider data), and it displays only a sanitized envelope.
3. To remove it, open `chrome://extensions` and choose **Remove**. Closing the browser also clears its in-memory observations.

## Safety boundary

The page-world observer only recognizes same-origin usage responses and completion/retry completion SSE data. The opaque route segment stays in page world. It sends the content script only an allowlisted, validated envelope; the extension does not store live observations, inspect credentials, log raw data, make network requests, modify responses, or delay Claude requests.

Run focused synthetic tests from the repository root:

```bash
npx vitest run tools/local-validation/claude-acquisition-probe/probe.test.js
```
