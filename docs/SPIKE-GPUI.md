# Spike: GPUI feasibility, step 1

**Question:** can GPUI open a desktop window that shows a real Marius vault,
and does it hold up at the scale the specification asks for?

**Answer: yes on macOS, with one large unknown untouched (the editor) and one
unanswered (Windows).** Details below.

Requirements in play: ARCH-002, ARCH-003, UI-001, UI-008, NFR-005, and the
specification's open question 1.

Spike code: `apps/desktop/` — **throwaway**. It is deliberately not a workspace
member, so a GPUI breakage can never fail `cargo test --workspace`. Build it
with `cargo run --manifest-path apps/desktop/Cargo.toml -- <vault>`.

---

## What was measured

Machine: Apple Silicon, macOS 25.6, Rust 1.99.1. `gpui` 0.2.2 from crates.io.
**All GPUI figures are from a debug build**, so they are conservative.

| Question | Result |
|---|---|
| Does `gpui` build from crates.io? | **Yes.** 506 compiled units, clean. No nightly, no system dependencies, no vendoring. |
| Build cost | ~9 minutes cold; **2.5 GB** of build artefacts. |
| Does a window open? | **Yes.** Registered with the window server as `marius-desktop-spike`, process idle on its event loop. |
| Does it show real vault data? | **Yes**, through `marius-core`'s ordinary service API — the same one the CLI uses. No new plumbing was needed. |
| Real load: 10,004 documents across 2 vaults | **930 ms** (debug), including opening the database and reconciling. |
| List at 10,000 rows | load 3.6 ms, **68 MB** RSS |
| List at 100,000 rows (NFR-005) | load 42 ms, **87 MB** RSS |
| Licence | Apache-2.0 — compatible with ARCH-004. |

**The 100,000-row result is the important one.** Ten times the rows cost 19 MB
more, which is roughly the size of the row data itself — so `uniform_list` is
virtualising properly and render cost is independent of list length. NFR-005's
100,000 documents are not a problem for the list.

## What the code looked like

About 190 lines for a window, a header, a virtualised list with alternating
row colours, click-to-select, and a detail footer. The API is declarative and
readable:

```rust
div().flex().flex_col().size_full().bg(rgb(0x1b1d1e))
    .child(header)
    .child(uniform_list("documents", count, cx.processor(|this, range, _window, cx| {
        range.map(|i| div().id(i).child(this.rows[i].title.clone())
            .on_mouse_down(MouseButton::Left, cx.listener(move |this, _, _, cx| {
                this.selected = Some(i);
                cx.notify();
            }))
            .into_any_element()).collect()
    })))
```

**One compile error, not obvious from the API shape:** the `uniform_list`
processor outlives `render`, so capturing the outer `cx` fails the borrow
check. The closure gets its own `cx`, which must be used instead. The error
message pointed straight at it. One iteration, no documentation needed.

## Findings worth acting on

1. **A `gpui` dependency costs 2.5 GB of build artefacts and ~9 minutes cold.**
   Keeping the UI out of the main workspace is worth doing permanently, not
   just for the spike: core development should not pay that.

2. **`WindowOptions` asked for 900×640 and got 810×605.** Harmless, but it
   means window sizing is not literal and a layout cannot assume it.

3. **Two of GPUI's transitive dependencies (`block` 0.1.6,
   `proc-macro-error2`) emit future-incompatibility warnings.** Not actionable
   by us, but worth watching.

## What this spike did *not* answer

- **The editor.** Still the largest risk by a wide margin. GPUI gives you
  layout, painting and input — not a text editor. Zed's editor is a large
  Zed-specific crate, not a reusable widget. MD-001/002/003 want source
  editing, live preview, highlighting, undo and crash recovery. Nothing here
  tells us what that costs.
- **Windows.** Not testable from macOS: the bundled SQLite needs a
  Windows-targeting C toolchain. `.github/workflows/ci.yml` runs the core and
  CLI on `windows-latest`; the GPUI half needs a real Windows machine.
  **This is now the blocking question**, because the product has to run at
  work.
- **iOS.** `gpui` 0.2.2 declares target-specific dependencies for macOS,
  Windows and Linux/FreeBSD and **none for iOS**, which is strong evidence it
  is not a supported target. ARCH-003's "thin native-UI fallback" looks like
  the likely answer.
- **The graph canvas.** Custom painting and hit-testing at thousands of nodes
  (GRAPH-001, GRAPH-014) was not attempted.
- **Scrolling feel.** Memory says the list virtualises; only a human with the
  window in front of them can say whether it *feels* right.

## Recommendation

Proceed with GPUI on macOS. It builds, it works, it talks to the real core
without adaptation, and it scales past the specification's own target.

Before committing further, run two more legs:

1. **Windows, on the work machine.** `cargo run --manifest-path
   apps/desktop/Cargo.toml -- <vault>`. A day at most, and it is the blocking
   question.
2. **The editor.** Time-boxed: can a Markdown editing surface with
   highlighting and undo be built on GPUI's text primitives in a week? If not,
   that reshapes the roadmap regardless of how good the rest is.

## Reproducing

```bash
cargo run --manifest-path apps/desktop/Cargo.toml -- /path/to/vault
cargo run --manifest-path apps/desktop/Cargo.toml -- --synthetic 100000
```
