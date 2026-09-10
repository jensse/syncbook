# syncbook -- notes for README.md

Raw steps and data collected while building this, kept here so nothing gets
forgotten while the real README.md gets written by hand. Not meant to be the
final doc -- reorganize/rewrite freely.

---

Pull reMarkable tablet notebooks over SSH and render them to PNG/SVG; push a
modified page back, safely (backup first, hash-verify after).

No reMarkable cloud account, no desktop app, no USB required if your tablet
is reachable over Wi-Fi/LAN -- just SSH.

## Why

The reMarkable stores each notebook as a directory of pages in a
reverse-engineered binary format ("lines v6"). `syncbook` doesn't reimplement
that format itself -- it shells out to the real `ssh`/`scp` for transport
(so it inherits your normal host-key handling and `ssh-agent`, nothing
reimplemented) and to [`rmc`](https://github.com/ricklupton/rmc) /
[`cairosvg`](https://cairosvg.org/) via [`uv`](https://astral.sh/uv) for
rendering. `syncbook` itself is just the orchestration: find the notebook,
move the right files, verify they arrived intact.

## Prerequisites

- A reMarkable tablet with SSH enabled (see below).
- [`uv`](https://astral.sh/uv) installed (`curl -LsSf https://astral.sh/uv/install.sh | sh`)
  -- used to run `rmc`/`cairosvg` without any manual `pip install` step.
- Rust (to build `syncbook` itself): `cargo build --release`.

## Enabling SSH on the reMarkable

1. On the tablet: **Settings -> About -> General/Software**, tap the entry
   near the bottom that shows a serial number and a one-time root password.
   This also shows the tablet's IP address on your current network (or use
   `10.11.99.1`, the fixed address reMarkable always assigns itself over
   USB).
2. From your computer, do one interactive login with that password to
   confirm connectivity:

   ```bash
   ssh root@<tablet-ip>
   ```

3. Install your public key so you never need the one-time password again:

   ```bash
   cat ~/.ssh/id_ed25519.pub | ssh root@<tablet-ip> \
     'mkdir -p ~/.ssh && chmod 700 ~/.ssh && cat >> ~/.ssh/authorized_keys && chmod 600 ~/.ssh/authorized_keys'
   ```

   (Generate a key first if you don't have one: `ssh-keygen -t ed25519`.)
   Note the source file before the pipe and the `chmod`s -- `sshd` will
   silently ignore `authorized_keys` if the permissions on `~/.ssh` or the
   file itself are too open.

4. Confirm the key-based login works before moving on:

   ```bash
   ssh -o PreferredAuthentications=publickey root@<tablet-ip> echo ok
   ```

## Building

```bash
cargo build --release
mkdir -p ~/bin
cp target/release/syncbook ~/bin/syncbook
```

Make sure `~/bin` is on your `PATH` (add `export PATH="$HOME/bin:$PATH"` to
your shell profile if it isn't already).

## First run

`syncbook` doesn't keep its own config file -- both `pullrm` and `pushrm`
connect via a normal `~/.ssh/config` host alias (default name: `remarkable`),
so plain `ssh remarkable` / `scp remarkable:...` on the command line behave
identically to what `syncbook` does internally.

Run any command; if the alias isn't configured yet, you'll be prompted once:

```
$ syncbook pullrm "My Notebook"
No 'remarkable' entry found in ~/.ssh/config -- first-time setup for syncbook.
reMarkable hostname or IP: 10.11.99.1
Path to SSH private key [/home/you/.ssh/id_ed25519]:
Wrote Host 'remarkable' to ~/.ssh/config. Re-run your command to continue.
```

This appends a block like:

```
# BEGIN SYNCBOOK MANAGED (remarkable)
Host remarkable
    HostName 10.11.99.1
    User root
    IdentityFile /home/you/.ssh/id_ed25519
    StrictHostKeyChecking accept-new
# END SYNCBOOK MANAGED (remarkable)
```

Edit that block directly (e.g. to change the IP after switching from USB to
Wi-Fi) -- `syncbook` only writes it once, on first run.

If you already have a working SSH alias for your tablet under a different
name, just pass `--host <name>` instead of renaming anything:

```bash
syncbook --host my-existing-alias pullrm "My Notebook"
```

## Usage

Pull a notebook by its visible name (as shown in the reMarkable UI) or by
UUID:

```bash
syncbook pullrm "My Notebook"
# -> ./My_Notebook/<page-uuid>.rm, .svg, .png for each page, plus content.json
```

Push a modified page back (backs up the current on-device state first,
verifies the upload by hash afterward):

```bash
syncbook pushrm "My Notebook" <page-uuid> ./modified-page.rm
```

**Important**: the reMarkable's own app (`xochitl`) only re-parses a page
from disk when its notebook is opened -- there is no live-reload signal to
send. After `pushrm` succeeds, open (or close and reopen) the notebook on
the tablet to see the change. Don't restart `xochitl` itself
(`systemctl restart xochitl`) to "force" a refresh -- during development of
this tool that caused a spurious blank page to appear, most likely because
the currently-open document's in-memory state and the file on disk briefly
disagreed across the restart. Simply opening the notebook is both safer and
sufficient.

## Backups

Every `pushrm` backs up the notebook's current `.metadata`, `.content`, and
the target page's current `.rm` (if it exists) to
`~/remarkable-backups/<UTC-timestamp>/<notebook-uuid>/` before touching
anything on the device. Nothing is deleted automatically -- prune old
backups by hand whenever you like.

## Advanced: writing native ink annotations

`pushrm` will happily upload any valid `.rm` file, but *constructing* one
that adds new pen strokes to an existing page -- e.g. to draw a native ink
annotation on top of someone's sketch -- is real surgery on a
reverse-engineered CRDT format, not something this tool automates. It's
documented as a worked example instead of a polished subcommand, because
getting it right requires understanding two non-obvious things:

1. **New content should go on its own layer**, with its own freshly
   registered author ID -- see `examples/annotate.py`, which uses
   [`rmscene`](https://github.com/ricklupton/rmscene) to build a `Group`,
   register it under the page's root, and add a `Line` to it.
2. **A new CRDT sequence item must not fabricate a causal link to existing
   content.** The natural-seeming approach -- pointing your new layer's
   `left_id` at an existing item's ID, to insert it "after" that item --
   gets silently accepted by `rmscene`'s reader (which is lenient) but
   **rejected by the real reMarkable app** with `rm.crdt.item Item: invalid
   origin left: ... / rm.crdt.sequence - invalid item` in
   `journalctl -u xochitl`, because you never actually observed that item's
   insertion the way a real collaborating client would have. The correct,
   honest representation of an independently-added layer is
   `left_id = right_id = CrdtId(0, 0)` -- a concurrent, uncoordinated
   insert, which is exactly what real independent edits are. Using a
   fabricated reference caused this exact failure during development, and
   the app's defensive recovery from it produced a stray blank page in the
   notebook as a side effect.

Run it like:

```bash
python3 examples/annotate.py path/to/page.rm path/to/page-annotated.rm
syncbook pushrm "My Notebook" <page-uuid> path/to/page-annotated.rm
```

You'll need to adjust the circle's center/radius in the script to match
where you actually want to annotate -- it's a worked example, not a general
tool (finding a good general-purpose UI for "here's where to annotate" is
future work, tracked as a shape/coordinate input problem, not a syncbook
problem).

## License

MIT, see [LICENSE](LICENSE).
