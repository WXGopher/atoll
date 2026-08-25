# Roadmap

This is the working list of what Atoll does next. It is a note to whoever picks
the project up rather than a schedule: nothing here carries a date, and the
order is closer to "what is worth doing" than to "what happens first".

## Done

- **The readout, the panel and the tray icon.** A dot per agent and its tightest
  rate-limit window, inside the taskbar, parked just clear of the notification
  area and moving along on its own as the tray grows. Behind it, a panel listing
  every session Atoll is tracking and every rate-limit window either agent has
  reported. The same panel opens from the tray icon, and the readout re-attaches
  by itself when the shell restarts.
- **Approvals answered from the card.** A card opens beside the readout when
  Claude Code is about to ask a human, and Allow, Deny or an answer to a question
  goes back to the live session. Tool calls the user's own settings already allow
  never raise a card, a card that has been seen collapses on its own, and a card
  dragged somewhere better stays there.
- **Hooks that are only ever added to.** `atoll setup install claude` copies the
  binaries somewhere stable and appends its hooks alongside whatever is already
  there; `status` says what is wired up and whether the transcripts are readable;
  `uninstall` takes back exactly what was added. `atoll headless` runs the same
  pipe server with no windows and prints the event stream.
- **Usage that costs almost nothing.** Claude Code's windows come from the same
  OAuth endpoint its own tooling reads, reusing any reading another tool on the
  machine already fetched, and a 429 puts the fetcher to sleep rather than into a
  retry loop. Codex's windows come from its rollout files.
- **Settings, a right-click menu, and machines that run one agent.** Right-click
  the readout for Settings and Quit. The settings window turns run-at-login on
  and off, shows or hides each agent's block in the readout, and moves the two
  colour thresholds. The readout carries a task line per agent, and a machine
  with only one agent installed gives the other's rows back to the taskbar.
- **Releases anyone can check.** Zips are built and published by the release
  workflow on GitHub's own runners, each with its SHA-256 and a signed build
  provenance attestation tying it to the commit and workflow run that produced
  it.

## Next: platform work

The three things the README calls planned, in roughly the order they are likely
to land.

- **Codex hook installation.** `atoll setup install codex` currently fails with
  "not implemented yet". What it needs is `~/.codex/config.toml` and its hooks
  file, wired with the same add-only, take-back-exactly-what-was-added
  discipline the Claude Code side already has. Until then Codex shows up only
  through the rollout files Atoll reads anyway, which is enough for rate limits
  and nothing else, and the settings page says "coming soon" beside a Codex it
  can already see on the machine.
- **Jumping back to a session's terminal.** The wire already carries most of
  what this needs: the hook forwards the terminal environment variables it can
  see, along with its own process id, on every event. What is missing is walking
  the parent chain from the hook to the window that owns it when those variables
  are not enough to identify it, and then bringing that window forward —
  Windows Terminal and VS Code first. The panel's session rows are deliberately
  read-only until that exists, because a row that looks clickable and does
  nothing is worse than a row that does not.
- **Toast notifications.** A card appears when a session wants an answer.
  Nothing appears when a long run finishes with nothing to ask, which is the
  other moment worth interrupting somebody for. This wants a toast, and a switch
  to turn it off, and enough restraint that the toast never arrives for work the
  user is already watching.

## Refinements under consideration

Everything below is currently a constant in the code that has not yet needed
changing. They are candidates, not commitments. Each one costs a setting
somebody has to understand, and the present default is the right answer on most
machines, so the bar for opening one up is a concrete complaint rather than a
suspicion that it might be wrong.

- **Refresh cadence.** A usage reading is reused for thirty seconds, and opening
  the panel accepts one no older than fifteen. Both are tuned for a machine
  where some other tool is also fetching; a machine where Atoll is the only
  reader could afford to be slower still.
- **Backoff after a failed fetch.** A rate-limited fetch waits two minutes, a
  transient failure fifteen seconds, and a missing token ten. These are the
  numbers most likely to want tuning per network rather than per taste.
- **Margins and sizes.** The gap the readout keeps from the notification area,
  the panel's width, and the card's. A dense taskbar and a 4K one do not want
  the same numbers.
- **How long a card lingers.** A card collapses shortly after the session moves
  on, and a card being hovered gets a longer stay of execution. Somebody who
  works with the card as a running log would want both longer.
- **Staleness and expiry.** An approval nobody answered is dropped after three
  minutes, on the reasoning that the agent has long since fallen back to
  prompting in its own terminal, and a session with nothing to say goes quiet
  after fifteen. Both would be worth exposing if a slow machine or a long
  approval turned either into a card that vanishes too early.

## Known rough edges

- **The right-click menu ignores dark mode.** The menus on the readout and the
  tray icon are classic Win32 menus, which render in the system's light theme
  regardless of what the rest of Atoll is doing. Making them follow dark mode
  means undocumented uxtheme calls, which is a real cost for a cosmetic fix.
- **The pipe carries the default security descriptor.** Another local account
  can open a client handle and inject synthetic events. It cannot impersonate
  the server or forge an approval — the reply path runs the other way — so this
  is noise injection rather than privilege escalation, but the pipe should be
  bound with an explicit DACL granting only the current user.
- **The screenshots in the README are behind the UI.** They predate the task
  lines in the readout and the current settings page, and want retaking.
- **The bring-up log stamps events in UTC.** Local time needs either a date-time
  dependency or a `GetLocalTime` call, and neither has been worth it for a log
  that is only read while something is broken.
