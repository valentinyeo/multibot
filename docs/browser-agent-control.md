# Control the docked browser from a VPS session

Open the session's browser pane in Multibot on Windows with Alt+A. Use the current
`multibotd` binary on its connected VPS. No Chrome extension, Native Messaging,
registry entry, public port, or extra service is needed.

```sh
~/.multibot/multibotd browser --session <session-id> navigate https://example.com
~/.multibot/multibotd browser --session <session-id> tabs
~/.multibot/multibotd browser --session <session-id> activate <target-id>
~/.multibot/multibotd browser --session <session-id> text
~/.multibot/multibotd browser --session <session-id> click '#submit'
~/.multibot/multibotd browser --session <session-id> type '#search' 'search terms'
~/.multibot/multibotd browser --session <session-id> eval 'document.title'
~/.multibot/multibotd browser --session <session-id> screenshot \
  | jq -r '.value.data' | base64 -d > page.png
~/.multibot/multibotd browser --session <session-id> cdp Page.reload '{}'
```

Use the session ID from the daemon's session list, not its display title. Commands
return one JSON `BrowserResponse`. Errors also exit nonzero. `type` replaces the
field's contents and emits input/change events. Screenshot data is a base64 PNG of
the viewport. The last activated tab is remembered for subsequent commands.

## Session boundaries

Each docked browser has its own Chromium process and profile. The desktop reads
its ephemeral loopback port and unique browser WebSocket path from
`DevToolsActivePort`. Commands travel over the existing SSH daemon connection;
Chrome's debugging port is never forwarded to the VPS or exposed publicly.

The daemon registers a binding for each open pane. It routes commands and replies
by both session ID and binding ID, not by whichever chat is selected. A target ID
must occur in that browser's own page list. Raw CDP accepts only a small allowlist
of page-level methods; browser-wide methods and explicit target/session routing
are rejected. This is not a multi-user security sandbox: VPS processes under the
same Unix account may select any registered session ID.

Closed, failed, or hibernated panes are unavailable. Reopen the pane before retrying.
If the same session has panes in two windows, close the extra pane. If two desktop
connections on the VPS own that session, select one with `--daemon <pid>` immediately
after `--session <session-id>`; the PID is the connected daemon's PID, not Chrome's.

## Resource limits

The bridge is event-driven and does no idle polling. Each daemon uses a private
0600 Unix socket under `~/.multibot/browser-control`, inside a 0700 directory.
Requests are limited to 64 KiB, replies to 512 KiB, and four requests can be in
flight. Large results fail with an error. Narrow text queries, or use raw
`Page.captureScreenshot` with a smaller `clip` for large pages. Browser commands
time out rather than holding a disconnected client indefinitely.
