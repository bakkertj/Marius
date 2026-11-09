# Desktop interaction testing (macOS)

Nothing can type into a GPUI window from `cargo test`, so interactive
behaviour is verified by driving the real application with synthesized
keyboard events and asserting on **files on disk** — the system of record —
plus window captures.

## Tools

- `sendkeys.swift` — posts keyboard events to the frontmost app.
  `swiftc -O -o /tmp/sendkeys sendkeys.swift`, then:

  ```
  /tmp/sendkeys --activate <pid> \
      --cmd k --sleep 0.4 --text "pharos" --key return \
      --cmd s
  ```

  Requires the invoking terminal to be granted **Accessibility**
  (System Settings → Privacy & Security → Accessibility). Exits 2 when not
  trusted, 3 when the target app cannot be brought frontmost.

- `findwin.swift` — lists on-screen windows (`id pid owner WxH`), for
  `screencapture -l<id>` captures regardless of z-order.

## Three macOS facts this harness had to learn the hard way

Each of these produced symptoms indistinguishable from an application bug,
and cost a debugging round before the trace logs pinned it on the platform:

1. **App activation is cooperative.** `NSRunningApplication.activate()` is a
   request, not a command, on macOS 14+. Post keystrokes immediately after it
   and they land in whatever is still frontmost — the harness polls
   `frontmostApplication` until the target really is front, and fails loudly
   if it never is.

2. **Unicode-string key events are unreliable; virtual-key events are not.**
   Events built with `keyboardSetUnicodeString` are dropped until the target
   process's text-input session is warm, with no error and no observable
   signal. Events built from real virtual keycodes were delivered in **every**
   run of every experiment. The harness therefore types ASCII through a US
   QWERTY keycode table (shift handled) and uses unicode strings only for
   characters that have no keycode.

3. **Assert on the filesystem, not the screen.** A screenshot proves pixels; a
   `grep` against the vault proves the save pipeline, the revision log and
   the index. Every gauntlet step ends in a disk assertion, with captures as
   corroboration.

## What was verified this way (2026-11-09)

Against a live window, with every assertion on disk:

- ⌘K → type a query → list filters live, snippets highlighted
- Enter opens the top hit **and** hands focus to the editor
- typing lands at the cursor; `#tags` highlight as typed
- ⌘S saves: file content updated atomically, revision recorded, reindexed —
  a `tag:` query finds the just-typed tag seconds later
- list continuation: Return inside `- item` produces the next bullet
- ⌘Z undoes by word-group, and a save after undo reaches the disk
- ⌘N creates a note named from the search box, on disk, and focuses it
- Escape loops editor → search (stale query selected for replacement)
- ⌘⇧L switches dark/light live
