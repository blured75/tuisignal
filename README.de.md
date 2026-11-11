# tuisignal

[Français](README.md) · [English](README.en.md) · **Deutsch**

[Signal](https://signal.org)-Client für das Terminal (TUI) unter Linux: Nachrichten, Dateien und
Bilder senden und empfangen, in Einzel- oder Gruppenunterhaltungen.

Eine einzige Rust-Binärdatei, ohne Java und ohne signal-cli:

- Kryptografie: [libsignal](https://github.com/signalapp/libsignal), die offizielle Bibliothek
  von Signal;
- Kommunikation mit den Signal-Servern: [presage](https://github.com/whisperfish/presage),
  aufgebaut auf libsignal (Signal veröffentlicht für diesen Teil keine offizielle
  Client-Bibliothek);
- Oberfläche: [ratatui](https://ratatui.rs).

## Installation

Voraussetzungen: Rust (1.85+), `protoc`, `cmake`, ein C-Compiler und OpenSSL (für SQLCipher).
Unter Arch / Omarchy: `sudo pacman -S --needed rust protobuf cmake clang openssl`.

```sh
cargo install --path .     # installiert ~/.cargo/bin/tuisignal (Kompilierung: einige Minuten)
```

## Erster Start

```sh
tuisignal link             # zeigt einen QR-Code an
```

Auf dem Telefon: *Signal → Einstellungen → Verknüpfte Geräte → Neues Gerät verknüpfen*, dann den
QR-Code scannen. Der Computer wird zu einem verknüpften Gerät, wie Signal Desktop.

`link` bietet an, die lokale Datenbank mit einer Passphrase zu verschlüsseln (empfohlen); sie wird
bei jedem Start abgefragt.

```sh
tuisignal
```

Kontakte und Gruppen kommen nach einigen Sekunden vom Telefon (`F5`, um sie erneut anzufordern).
Senden ist möglich, sobald der Titel „● connecté“ (verbunden) anzeigt.

## Bedienung

| Taste                 | Aktion                                                          |
|-----------------------|-----------------------------------------------------------------|
| `↑` `↓` `Enter`       | Unterhaltung auswählen und öffnen                               |
| Text eingeben         | in der Liste: Kontakte und Gruppen filtern (`Ctrl+R` kehrt dorthin zurück) |
| `Enter`               | Nachricht und wartende Dateien senden                           |
| `Alt+Enter`           | Zeilenumbruch in der Nachricht                                  |
| `Ctrl+O`              | Datei oder Bild anhängen (Dateiauswahl)                         |
| `Ctrl+P`              | Anhänge der Unterhaltung; `Enter` öffnet die Datei              |
| `PgUp` `PgDn` `↑` `↓` | durch die Nachrichten scrollen                                  |
| `Esc`                 | wartende Dateien entfernen / zur Liste zurückkehren             |
| `Tab`                 | von der Liste zum Eingabefeld wechseln                          |
| `F5`                  | Kontakte und Gruppen erneut vom Telefon anfordern               |
| `Ctrl+Q`              | beenden                                                         |

Um eine Datei anzuhängen, kann man sie auch per Drag & Drop ins Terminal ziehen (oder ihren
absoluten Pfad einfügen) oder `/attach ~/pfad/zur/datei` eingeben.

Bilder werden in der Unterhaltung angezeigt (Grafikprotokolle Kitty/Ghostty, Sixel, iTerm2;
sonst Unicode-Halbblöcke). Andere Dateien erscheinen als `📎 name` und lassen sich über `Ctrl+P`
mit der Standardanwendung (`xdg-open`) öffnen. Anhänge über 50 MB werden nur auf Anforderung über
`Ctrl+P` heruntergeladen.

Ebenfalls unterstützt: Gruppen, Zustell- und Lesebestätigungen (`✓`, `✓✓`, `✓✓ lu`), empfangene
Reaktionen, Zitate, bearbeitete oder gelöschte Nachrichten, von Ihren anderen Geräten gesendete
Nachrichten, automatische Wiederverbindung.

## Optionen

```
tuisignal [--notify] [--no-read-receipts] [--data-dir VERZEICHNIS]
tuisignal link [-n GERÄTENAME]
tuisignal unlink
```

- `--notify`: Desktop-Benachrichtigung (`notify-send`) bei jeder empfangenen Nachricht.
- `--no-read-receipts`: keine Lesebestätigungen senden.
- `unlink`: trennt das Gerät vom Konto und löscht die lokalen Daten.
- `TUISIGNAL_PASSPHRASE`: übergibt die Passphrase, ohne sie einzugeben.
- `TUISIGNAL_LOG=debug`: ausführliches Protokoll in `~/.local/state/tuisignal/tuisignal.log`
  (kann sensible Metadaten enthalten).

## Daten und Sicherheit

Alles liegt in `~/.local/share/tuisignal/` (Rechte `700`):

| Datei           | Inhalt                                          | Schutz                                  |
|-----------------|-------------------------------------------------|-----------------------------------------|
| `signal.db`     | Kontoschlüssel, Sitzungen, Kontakte, Nachrichten | verschlüsselt (SQLCipher), falls Passphrase gesetzt |
| `attachments/`  | empfangene und gesendete Anhänge                | **unverschlüsselt**                     |
| `state.json`    | Unterhaltungs-IDs, Zeitstempel, Ungelesene      | unverschlüsselt, kein Nachrichtentext   |

Die Anhänge bleiben unverschlüsselt, damit andere Anwendungen sie öffnen können: Eine
verschlüsselte Festplatte (LUKS) wird weiterhin empfohlen.

Der Verlauf enthält nur die seit der Verknüpfung empfangenen Nachrichten: Signal speichert nichts
auf dem Server.

## Einschränkungen

Keine Anrufe, keine Storys, kein Erstellen/Verwalten von Gruppen und kein Senden von Reaktionen.
Neue Kontakte werden automatisch akzeptiert (kein Bildschirm zur Überprüfung der
Sicherheitsnummer: Bitte am Telefon durchführen).

## Entwicklung

```sh
cargo test      # Nachrichtenmodell und Oberfläche (simuliertes Terminal)
cargo run
```

- `src/model.rs`: Rekonstruktion der Unterhaltungen aus den von presage gespeicherten Inhalten;
- `src/signal.rs`: Empfangen, Senden, Anhänge;
- `src/ui.rs`: Oberfläche;
- `src/sidecar.rs`: Lese- und Zustellstatus;
- `src/main.rs`: Kommandozeile, Verknüpfung.

## Lizenz

presage steht unter der AGPL-3.0-Lizenz: Jede Weitergabe von tuisignal muss ebenfalls unter
dieser Lizenz erfolgen.
