# Changelog

User-facing changes, newest first. Release numbers match published GitHub releases.
Changes on main that are not yet published belong under Unreleased. When a release
ships, move those bullets under its published version; keep older entries here.
This file is embedded in Multibot so Changelog works offline.

## Unreleased

- Hide tool calls behind a compact expandable chip; show or hide all details through Ctrl+K → Chat: Show / hide tool call details.
- Open chat links with a click and terminal or native shell URLs with Ctrl+click; only HTTP and HTTPS links can open.
- Use the classic robot icon for the Windows app, title bar, taskbar, installer, and shortcuts.
- Start a new server shell from Ctrl+K with Session: New Shell session, choosing a directory when the workspace has several.
- Paste Windows clipboard text into focused server shells with Ctrl+V or Ctrl+Shift+V instead of asking remote tools to read a clipboard.
- Open new chats without missing-history errors, including Antigravity before its first prompt.
- Dissolve a server shell with two Ctrl+Shift+W presses within three seconds to close its terminal and stop its remote tmux session.
- After Alt+E, select the next chat down; after Ctrl+Shift+W, select the chat above, without reordering the sidebar.
- Show real tmux window titles for server shells and refresh them when renamed, while preserving custom Multibot and legacy names.
- Resolve available legacy shell names, including host-qualified terminal keys, while keeping Multibot renames.
- Remember shell full-width mode across restarts; toggle it with Ctrl+Shift+F or Ctrl+K.
- Keep sidebar developer stats in a tiny indicator; click to expand GPU, memory, CPU and connection details, or press Esc to collapse.
- Show sidebar section names without collapse arrows; clicking a heading keeps chats visible, and chats follow newest messages without moving on selection.
- Pin chats above all sections and return unpinned chats to their section, preserving pin choices across imports and in Starred; right-click any chat for its Ctrl+K actions.
- Switch workspaces with Alt+Shift+W, the sidebar header, or Ctrl+K → Workspaces. The palette replaces the sidebar dropdown; matching sessions stay above workspaces in main search.
- Dock Chrome or Edge beside each chat with Alt+A, switch focus with Alt+Q, and attach a browser screenshot with Ctrl+Shift+S.
- Dissolve a workspace to stop its agents and keep chats grouped in Dissolved; reactivating a chat restores its workspace. Delete workspace still keeps agents running and moves chats to General.
- Open lifecycle views with single chords or headline tabs, without an Alt+G leader. Hibernate now shows Alt+H first; Ctrl+H still works.
- Keep shell discovery working when the tmux server starts alongside Multibot.
- Reduce server memory use when reading Pi and Cursor chats with long replies or images.
- Open Changelog from Ctrl+K to read the full release history, even offline.
- Discover live tmux server shells and use the built-in Windows terminal. Escape stays with the terminal while it is visible.
- Toggle Deepgram dictation with Ctrl+D as well as Ctrl+Shift+Space. Multibot can load DEEPGRAM_API_KEY from your environment.
- Use Pi and Cursor agents alongside the existing agents on your server.
- Choose a working directory when starting an agent in a workspace with multiple directories.
- Sign in at account.multiprompt.dev to sync your Deepgram key.
- See your custom bot faces imported from legacy Multiprompt.

## v0.1.55

- Clicking a chat or selecting it with Ctrl+1..9 keeps the sidebar order unchanged.

## v0.1.54

- Chats open at the latest message and follow new messages while you are at the bottom.
- A NEW divider marks unread messages. Jump to the latest message with the floating button or Shift+GG.

## v0.1.53

- Cycle through all sidebar chats with Ctrl+Tab and Ctrl+Shift+Tab without changing their order.

## v0.1.52

- Set chat reminders with Ctrl+H and snooze workspaces with Ctrl+Shift+Y.

## v0.1.51

- Dictate into the composer with Deepgram using Ctrl+Shift+Space.

## v0.1.50

- Install Multibot with the Windows Setup installer and launch it from the Start Menu.
- Download, install, and relaunch an available update with one in-app action.

## v0.1.49

- Read real Antigravity conversation history and resume live chats without losing responsiveness.

## v0.1.48

- Connecting to a server no longer replaces a newer Multibot daemon with an older copy.

## v0.1.47

- Chats appear in the workspace recorded by the server, including newly created chats.
- Show custom chat names, including names from the legacy app.

## v0.1.46

- Load and show active sessions only, keeping the sidebar focused and reducing memory use.
