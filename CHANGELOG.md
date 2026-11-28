# Changelog

User-facing changes, newest first. Release numbers match published GitHub releases.
Changes on main that are not yet published belong under Unreleased. When a release
ships, move those bullets under its published version; keep older entries here.
This file is embedded in Multibot so Changelog works offline.

## Unreleased
- Add Browser: Sign in to Google in Ctrl+K to open a clean Chrome window on the shared Multibot profile without remote debugging flags (MBRU-140).
- Automatically load older chat history when scrolling near the top and backfill sparse Antigravity chats on open (MBRU-138).
- Match queued Send now to the composer’s up-arrow icon and use compact, borderless buttons for queued message actions (MBRU-129).
- Align Main Chat controls and sidebar edges, widen the reading column, fit short message bubbles to their text, and keep long messages and table controls inside narrow windows (MBRU-112).
- Send chat messages with Enter, add line breaks with Shift+Enter, and force send queued messages with Ctrl+Enter (MBRU-137).
- Add Ctrl+Shift+Enter to force send queued messages and show keyboard shortcuts in mouse-over tooltips on icon buttons (MBRU-136).
- Detect live subworkers in Antigravity sessions using the conversation flag and sibling processes (MBRU-135).
- Give each live server shell session its own native console window so switching between full-screen btop and other shells is instant without reloads or cross-session bleed (MBRU-134).
- Fix the docked Windows browser pane to fill the right side with full native Chrome using a shared Multibot profile, removing the duplicate address bar and buttons (MBRU-132).

- Hide hibernated chats from the sidebar and active view immediately after choosing a time in Alt+H (MBRU-133).
- Autocomplete natural language durations and dates with resolved date and time previews in Alt+H Hibernate (MBRU-131).
- Show each session’s live worker count in the sidebar and chat, with expandable tickets, folders, models, and elapsed time (MBRU-117).
- Keep live shell mouse input responsive, restore attached shells in All, and improve dictation at natural pauses (MBRU-118, MBRU-119, MBRU-120).
- Quote selected passages inline, reply directly with Enter, react with r, use message actions from Ctrl+K, and keep scrolling anchored during chat updates (MBRU-121, MBRU-122, MBRU-123).
- Fit image cards to their exact proportions and open screenshots in a dark in-app viewer; prevent Windows crashes when opening external links (MBRU-124, MBRU-125).
- Keep each chat’s unsent text, quotes, and attachments separate, and allow slash commands and skills anywhere after whitespace (MBRU-127, MBRU-128).
- Pin live activity above the composer, expand steps upward, and remove premature no-reply notices and repeated activity verbs (MBRU-130).
- Align the Inbox tab and filter chip labels on the session card leading edge and hold every row timestamp on one fixed right-aligned column (MBRU-113).

- Discover enabled Codex plugins such as pstack in the slash and skill picker (MBRU-109).
- Deduplicate Antigravity sessions that share one native conversation so chats appear once in the sidebar (MBRU-110).
- Fetch and unfurl remote screenshot and image links inline in chat (MBRU-111).
- Condense live working status into one interactive bottom-left row with 5px animated dots, rotating plain-English activity, expandable current-turn steps, full Antigravity hidden activity under Show tool calls, and monochrome dark grey composer buttons (MBRU-116).
- Align the Command Palette search text, breadcrumb, section headers, and row labels on one leading edge (MBRU-114).
- Align the Settings sidebar title with the tab labels and centre the close button on the pane heading (MBRU-115).
- Keep the sidebar visible in full-width chats and resize live shell grids correctly when their pane changes size (MBRU-98, MBRU-99).
- Keep metadata-only chats idle, open empty chats without a welcome card, and hide Claude subagent transcripts from the sidebar (MBRU-100, MBRU-102).
- Remove the Inbox dot, unify circular controls, and float the version pill and controls over full-height sidebar and chat views (MBRU-101, MBRU-103).
- Keep hibernating chats hidden on reply by default and omit deleted server directories from directory pickers (MBRU-104).
- Default agents to full access and keep permission choices consistent between the composer + menu and Ctrl+K (MBRU-105).
- Make headings and secondary text easier to distinguish in Alt+H and other dialogs (MBRU-106).
- Prefer full Google Chrome windows for the docked browser while keeping GPU acceleration disabled (MBRU-107).
- Deliver @ mentions to sessions on other connected hosts (MBRU-108).

- Replace text action buttons in chat and Server shells with icons for stop, send now, edit, and remove (MBRU-93, MBRU-96).
- Enlarge the three-dot busy indicator so it is clearly visible in chat (MBRU-95).
- Keep sent messages visible until the transcript catches up, without showing queued messages as sent too early (MBRU-94).
- Fix Force Send so interrupted Antigravity and agent turns resume reliably, with the force-sent message visible and no stray SIGINT notice (MBRU-97).
- Keep live server shell lists updating after an early tmux connection failure.
- Share one SSH connection per server across live shells, switching instantly without opening extra console windows.
- Queue follow-up messages across Claude, Codex, Antigravity, Pi, and Cursor; edit, remove, or interrupt and send any queued message immediately.
- Show a visible Kill/Cancel confirmation for live server shells, with eight seconds to confirm Ctrl+Shift+W and Escape to cancel.
- Remember full-width mode per session across restarts, restoring the sidebar when opening a normal-width session.
- Open Ctrl+K → Server shells to attach or permanently end live shells across all connected servers, including detached shells.
- Recover Antigravity replies after daemon restarts by reading and watching the native conversation transcript instead of a separate message copy.
- Mention live sessions with the composer’s @ picker, keep duplicate names bound to the selected session, and click mentions to open that exact chat.
- List and message live sessions with `multibotd sessions list` and `multibotd sessions send`, including same-host mention delivery and bounded reply chains.

- Paste clipboard text reliably into live server shells with Ctrl+V or Ctrl+Shift+V, including multiline text.
- Scroll live server shells with the mouse wheel even when the app holds keyboard focus; preserve terminal PageUp and PageDown keys.
- Start Multibot maximized on every launch, including update restarts.
- Keep dissolved server shells dissolved after restarting the app or reconnecting.
- Hide tool calls completely by default; use Ctrl+K → Chat: Show tool calls to reveal compact, expandable details.
- Send chat messages with Ctrl+Enter; Enter and Shift+Enter add lines, while Enter still selects slash commands and opens a focused message’s menu.
- Let VPS agents control their session’s docked Chrome or Edge pane through `multibotd browser`, without an extension, Native Messaging, or registry changes.
- Navigate chat messages with Up and Down; press Enter for Copy, Reply, and Quote, or Esc to return to typing.
- Switch a chat between Claude, Codex, Antigravity, Pi, and Cursor from Ctrl+K or the composer + menu, keeping its name, workspace, directory, and conversation; configured Pi models are included.

- Keep new chats error-free before their transcript folders exist, while still reporting real read failures.
- Stop archive and restore confirmations from lingering above unrelated live chats.
- Keep closed Codex chats and unused transcripts out of the live sidebar; handle /exit and /quit without sending prompts.
- Cycle Frequent sessions in sidebar order with Ctrl+Tab and Ctrl+Shift+Tab, without changing workspaces or reshuffling the list.
- Show each sidebar session’s CLI abbreviation and working directory below its title, including pinned chats.
- Type immediately after switching chats or live server shells, without clicking the input first.
- Grow the chat composer upward as text wraps, up to ten lines, then scroll within it.
- Keep live server shells beside the docked browser or Computer View, with working keyboard focus in both panes.
- Search keyboard shortcuts by action, section, or key combination; F1 and Esc still close the modal.
- Follow the latest messages when typing, pasting, dictating, or sending; keep the new-message button only while reading earlier messages.
- Use a dark circular dictation mic, a recording timer pill, and a copper arrow send button.
- Open Chat Info from the chat or shell title, or Ctrl+K, to see the directory, runtime, model, effort, and reported context use; copy the directory in one click.
- Restore typing in Windows live shells after switching back to Multibot or clicking the shell, without taking focus from dialogs or other inputs.

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
