# Session mentions and messages

Type `@` at the start of the chat composer or after whitespace. The picker shows live sessions, their agent, directory, host, and session ID. Filter by name, workspace, or ID. Use Up and Down to choose, then Enter or Tab to insert. Escape closes the picker without clearing your draft.

Picked mentions remain bound to that exact session, including when names match. The composer shows `@Name`. The outgoing prompt carries `@Name{session_id}`. Chat messages hide the ID and show a clickable mention. Clicking opens the target session and its host and workspace.

Sending a mention also delivers the message to that session when it is on the same host. Mentions inside code blocks do not deliver messages. A target is notified once per prompt, even if mentioned repeatedly. Cross-host mentions open the other session but do not forward messages between servers.

## Agent and terminal commands

Connect Multibot to the server first. Its running daemon exposes a private local socket under `~/.multibot/sessions/`.

```sh
multibotd sessions list
multibotd sessions list --json
multibotd sessions send 'Postbox' 'Please check the latest build.'
multibotd sessions send 8ad9c3d6-0000-0000-0000-000000000000 'Please check the latest build.' --from sender-session-id
```

Use an exact session ID or a unique, case-sensitive display name. Ambiguous names are rejected with no delivery. JSON listings include `title`, `id`, `agent`, `cwd`, and `working`.

Agents and new shells launched by Multibot inherit `MULTIBOT_SESSION_ID`. The CLI uses it as the sender unless `--from` is provided. Other callers are labelled `CLI`. Messages arrive as `[Message from <sender>]: ...` with a depth header. Chat agents receive a prompt, queued behind an existing turn when needed. Shells receive literal text through tmux followed by Enter.

Reply chains and repeated deliveries are limited to four levels. Fresh message text does not reset this limit. A new user prompt in the affected session resets its limit; otherwise it expires after ten minutes. Messages are limited to 16 KiB. These limits prevent unattended agents from exchanging messages forever.

Desktop clients can request same-host delivery directly with the daemon protocol:

```json
{"op":"send_cross_session","from_session_id":"sender-id","target_session_id":"target-id","text":"Please check the latest build."}
```

The daemon replies with `send_accepted` or `error` with code `cross_session_failed`. An acknowledgement means the message was dispatched or queued, not that the agent has answered.
