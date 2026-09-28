# Remote Filesystems (VFS)

TermIDE includes a pure-Rust VFS layer that lets the [file
manager](file-manager.md) browse remote servers as if they were local
directories. Files can be opened in the editor, copied between local
and remote panels, renamed, and so on — the same UI, with progress
shown in the [Operations panel](operations.md).

No native libraries are required: SFTP runs on `russh` + `russh-sftp`,
FTPS on `rustls`. Builds work statically on Alpine / musl out of the
box.

## Supported protocols

| Scheme  | Protocol                                          |
|---------|---------------------------------------------------|
| `sftp://` | SSH File Transfer Protocol over SSH             |
| `ftp://`  | Plain FTP                                       |
| `ftps://` | FTP over TLS (rustls)                           |

`smb://` and `nfs://` are recognised by the URL parser but the
providers behind them are not currently shipped — use them via the
operating system's native mount instead.

## URL syntax

A VFS URL has the form:

```
scheme://[user[:password]@]host[:port][/path]
```

Examples:

```
sftp://nvn@example.com/home/nvn/projects
sftp://example.com:2222/srv/builds
ftp://files.example.com/pub
ftps://secure.example.com/uploads
```

Notes:
- Non-ASCII characters in `path` are accepted; the parser
  percent-decodes them back to UTF-8 before talking to the server.
- Omitting the user makes the SFTP provider fall back to your SSH
  config (see Authentication).
- Omitting the port uses the protocol default (22 / 21 / 990).
- Embedding a password in the URL is supported but not recommended —
  prefer key-based authentication for SFTP.

## Opening a remote location

Two ways to get a remote panel open:

1. **Go to path** — open the file manager's `Go to path` input and
   paste / type a VFS URL.
2. **Bookmarks** — store frequently-used remotes once and reach them
   through the `Bookmarks` menu. Bookmark entries accept the same URL
   syntax as above; they're stored in
   `~/.config/termide/bookmarks.toml` (or the project-local
   `.termide/bookmarks.toml` if you keep team-shared shortcuts in the
   repo).

Both paths land you in a normal file manager view rooted at the
remote directory; everything else (tree expand, copy, rename,
properties on Space) is exactly the same as for local panels.

## Authentication (SFTP)

SFTP supports four authentication modes:

- **`Auto`** *(default)* — tries SSH agent first, then keys listed in
  `~/.ssh/config` for the host (including `IdentityFile`, `User`,
  `Port` and `Hostname` aliases), then default keys
  (`id_ed25519`, `id_rsa`, `id_ecdsa`, `id_dsa`), then password.
- **SSH agent** — uses an SSH agent if `SSH_AUTH_SOCK` points at one.
- **SSH key** — explicit private-key file, optionally with a
  passphrase.
- **Password** — interactive prompt or pre-stored value.

Because `Auto` reads `~/.ssh/config`, you can keep the bookmark URL
plain (`sftp://my-build-host/path`) and let SSH config supply the
real hostname, user and key file. Same configuration as the `ssh`
CLI uses.

## How remote panels feel

Remote operations are all asynchronous:

- The first directory listing shows a brief spinner; entries appear
  when the server replies.
- Expanding a subdirectory inserts a tiny `…` placeholder; once the
  listing arrives it's swapped for the real children.
- Tree state, cursor and selection are preserved across reloads.

Wide-view directory sizes are **not** computed for remote panels —
the cost of walking a remote tree just for a column would dwarf the
benefit. Remote size always shows blank.

## Transferring files

Pick files in one panel, hit `C` / `F5` (copy) or `M` / `F6` (move),
and choose a destination — either a local path or another remote
panel. The transfer registers as an operation in the
[Operations panel](operations.md), with:

- A real progress bar (bytes + files), updated from the worker on
  each chunk.
- Pause / Resume that actually stops the byte stream (paused
  uploads / downloads sit idle on the worker side; the SFTP actor
  stays free for other panels' metadata requests).
- Cancel that stops cleanly between chunks. If the cancelled
  operation left a partial file on the server, the panel asks
  whether to delete it — see the cancel-cleanup section in
  [Operations panel](operations.md).

Same-host SFTP and FTP renames stay on the server: a move within one
connection issues a remote-side rename, not download-then-upload.

## Projects

A project layout that includes a remote file manager panel persists the
URL just like a local path. On the next start TermIDE reconnects in the
background and shows the panel with a loading placeholder until the
listing arrives — the rest of the UI is responsive immediately.

### Dropped connections

If the remote session is lost (idle timeout, network drop), the next
operation fails and the panel shows a recovery dialog with three choices:
**Reconnect** (open a fresh session to the same path), **Open home
(local)** (drop the connection and switch the panel to your local home
directory), or **Close panel**. Pressing `Esc` dismisses the dialog and
leaves the panel on its last listing. The panel never loops on the error.

## Limitations / known gaps

- No `smb://` / `nfs://` provider yet — only URL parsing.
- No resume-from-byte-offset for interrupted transfers: cancelled
  uploads start over from the beginning if re-issued.
- The interactive password prompt cannot be saved persistently
  inside TermIDE; for repeated use, configure SSH keys or rely on
  your SSH agent.
