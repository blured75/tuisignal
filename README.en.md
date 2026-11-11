# tuisignal

[Français](README.md) · **English** · [Deutsch](README.de.md)

Terminal (TUI) [Signal](https://signal.org) client for Linux: send and receive messages, files
and images, in direct or group conversations.

A single Rust binary, with no Java and no signal-cli:

- cryptography: [libsignal](https://github.com/signalapp/libsignal), Signal's official library;
- communication with the Signal servers: [presage](https://github.com/whisperfish/presage), built
  on libsignal (Signal does not publish an official client library for this part);
- interface: [ratatui](https://ratatui.rs).

## Installation

Requirements: Rust (1.85+), `protoc`, `cmake`, a C compiler and OpenSSL (for SQLCipher).
On Arch / Omarchy: `sudo pacman -S --needed rust protobuf cmake clang openssl`.

```sh
cargo install --path .     # installs ~/.cargo/bin/tuisignal (build takes a few minutes)
```

## First launch

```sh
tuisignal link             # displays a QR code
```

On your phone: *Signal → Settings → Linked devices → Link new device*, then scan the QR code.
The computer becomes a linked device, just like Signal Desktop.

`link` offers to encrypt the local database with a passphrase (recommended); it will be asked
for at every launch.

```sh
tuisignal
```

Contacts and groups arrive from the phone after a few seconds (`F5` to request them again).
Sending is possible as soon as the title shows "● connecté" (connected).

## Usage

| Key                   | Action                                                        |
|-----------------------|---------------------------------------------------------------|
| `↑` `↓` `Enter`       | select and open a conversation                                |
| type text             | in the list: filter contacts and groups (`Ctrl+R` returns there) |
| `Enter`               | send the message and pending files                            |
| `Alt+Enter`           | line break in the message                                     |
| `Ctrl+O`              | attach a file or image (file picker)                          |
| `Ctrl+P`              | conversation attachments; `Enter` opens the file              |
| `PgUp` `PgDn` `↑` `↓` | scroll messages                                               |
| `Esc`                 | remove pending files / return to the list                     |
| `Tab`                 | switch from the list to the input field                       |
| `F5`                  | request contacts and groups from the phone again              |
| `Ctrl+Q`              | quit                                                          |

To attach a file, you can also drag and drop it into the terminal (or paste its absolute path),
or type `/attach ~/path/to/file`.

Images are displayed inside the conversation (Kitty/Ghostty, Sixel and iTerm2 graphics
protocols; Unicode half-blocks elsewhere). Other files appear as `📎 name` and open from `Ctrl+P`
with the default application (`xdg-open`). Attachments larger than 50 MB are only downloaded on
demand, from `Ctrl+P`.

Also supported: groups, delivery and read receipts (`✓`, `✓✓`, `✓✓ lu`), incoming reactions,
quotes, edited or deleted messages, messages sent from your other devices, automatic
reconnection.

## Options

```
tuisignal [--notify] [--no-read-receipts] [--data-dir DIR]
tuisignal link [-n DEVICE_NAME]
tuisignal unlink
```

- `--notify`: desktop notification (`notify-send`) for every incoming message.
- `--no-read-receipts`: do not send read receipts.
- `unlink`: unlinks the device from the account and deletes local data.
- `TUISIGNAL_PASSPHRASE`: provides the passphrase without typing it.
- `TUISIGNAL_LOG=debug`: detailed log in `~/.local/state/tuisignal/tuisignal.log`
  (may contain sensitive metadata).

## Data and security

Everything lives in `~/.local/share/tuisignal/` (permissions `700`):

| File            | Contents                                        | Protection                              |
|-----------------|-------------------------------------------------|-----------------------------------------|
| `signal.db`     | account keys, sessions, contacts, messages      | encrypted (SQLCipher) if passphrase set |
| `attachments/`  | received and sent attachments                   | **plaintext**                           |
| `state.json`    | conversation IDs, timestamps, unread counts     | plaintext, no message text              |

Attachments are kept in plaintext so other applications can open them: an encrypted disk (LUKS)
is still recommended.

History only contains messages received since linking: Signal keeps nothing on the server side.

## Limitations

No calls, no stories, no group creation/management, and no sending of reactions. New contacts
are approved automatically (there is no safety number verification screen: do it from your
phone).

## Development

```sh
cargo test      # message model and interface (simulated terminal)
cargo run
```

- `src/model.rs`: rebuilds conversations from the content stored by presage;
- `src/signal.rs`: receiving, sending, attachments;
- `src/ui.rs`: interface;
- `src/sidecar.rs`: read and delivery state;
- `src/main.rs`: command line, linking.

## License

presage is licensed under AGPL-3.0: any redistribution of tuisignal must be as well.
