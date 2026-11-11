# tuisignal

Client [Signal](https://signal.org) en mode console (TUI) pour Linux : envoyer et recevoir des
messages, des fichiers et des images, en conversation directe ou en groupe.

Un seul binaire Rust, sans Java ni signal-cli :

- cryptographie : [libsignal](https://github.com/signalapp/libsignal), la bibliothèque officielle
  de Signal ;
- dialogue avec les serveurs Signal : [presage](https://github.com/whisperfish/presage), bâtie sur
  libsignal (Signal ne publie pas de bibliothèque cliente officielle pour cette partie) ;
- interface : [ratatui](https://ratatui.rs).

## Installation

Prérequis : Rust (1.85+), `protoc`, `cmake`, un compilateur C et OpenSSL (pour SQLCipher).
Sur Arch / Omarchy : `sudo pacman -S --needed rust protobuf cmake clang openssl`.

```sh
cargo install --path .     # installe ~/.cargo/bin/tuisignal (compilation : quelques minutes)
```

## Premier lancement

```sh
tuisignal link             # affiche un QR code
```

Sur le téléphone : *Signal → Paramètres → Appareils associés → Associer un nouvel appareil*, puis
scanner le QR code. L'ordinateur devient un appareil associé, comme Signal Desktop.

`link` propose de chiffrer la base locale par une phrase de passe (recommandé) ; elle sera
demandée à chaque lancement.

```sh
tuisignal
```

Les contacts et groupes arrivent du téléphone au bout de quelques secondes (`F5` pour relancer la
demande). L'envoi est possible dès que le titre affiche « ● connecté ».

## Utilisation

| Touche            | Action                                                          |
|-------------------|-----------------------------------------------------------------|
| `↑` `↓` `Entrée`  | choisir et ouvrir une conversation                              |
| taper du texte    | dans la liste : filtrer contacts et groupes (`Ctrl+R` y revient) |
| `Entrée`          | envoyer le message et les fichiers en attente                   |
| `Alt+Entrée`      | saut de ligne dans le message                                   |
| `Ctrl+O`          | joindre un fichier ou une image (sélecteur de fichiers)         |
| `Ctrl+P`          | pièces jointes de la conversation ; `Entrée` ouvre le fichier   |
| `PgUp` `PgDn` `↑` `↓` | faire défiler les messages                                  |
| `Échap`           | retirer les fichiers en attente / revenir à la liste            |
| `Tab`             | passer de la liste à la saisie                                  |
| `F5`              | redemander contacts et groupes au téléphone                     |
| `Ctrl+Q`          | quitter                                                         |

Pour joindre un fichier, on peut aussi le glisser-déposer dans le terminal (ou coller son chemin
absolu), ou taper `/attach ~/chemin/vers/fichier`.

Les images s'affichent dans la conversation (protocoles graphiques Kitty/Ghostty, Sixel, iTerm2 ;
demi-blocs Unicode ailleurs). Les autres fichiers apparaissent sous la forme `📎 nom` et s'ouvrent
depuis `Ctrl+P` avec l'application par défaut (`xdg-open`). Les pièces jointes de plus de 50 Mo ne
sont téléchargées qu'à la demande, depuis `Ctrl+P`.

Sont aussi gérés : groupes, accusés de réception et de lecture (`✓`, `✓✓`, `✓✓ lu`), réactions
reçues, citations, messages modifiés ou supprimés, messages envoyés depuis vos autres appareils,
reconnexion automatique.

## Options

```
tuisignal [--notify] [--no-read-receipts] [--data-dir DOSSIER]
tuisignal link [-n NOM_APPAREIL]
tuisignal unlink
```

- `--notify` : notification de bureau (`notify-send`) à chaque message reçu.
- `--no-read-receipts` : ne pas envoyer d'accusés de lecture.
- `unlink` : dissocie l'appareil du compte et efface les données locales.
- `TUISIGNAL_PASSPHRASE` : fournit la phrase de passe sans la saisir.
- `TUISIGNAL_LOG=debug` : journal détaillé dans `~/.local/state/tuisignal/tuisignal.log`
  (peut contenir des métadonnées sensibles).

## Données et sécurité

Tout est dans `~/.local/share/tuisignal/` (droits `700`) :

| Fichier         | Contenu                                         | Protection                          |
|-----------------|-------------------------------------------------|-------------------------------------|
| `signal.db`     | clés du compte, sessions, contacts, messages    | chiffré (SQLCipher) si phrase de passe |
| `attachments/`  | pièces jointes reçues et envoyées               | **en clair**                        |
| `state.json`    | identifiants de conversations, horodatages, non lus | en clair, aucun texte de message |

Les pièces jointes restent en clair pour pouvoir être ouvertes par d'autres applications :
un disque chiffré (LUKS) reste recommandé.

L'historique ne contient que les messages reçus depuis l'association : Signal ne conserve rien
côté serveur.

## Limites

Pas d'appels, de stories, de création/gestion de groupes, ni d'envoi de réactions. Les nouveaux
contacts sont approuvés automatiquement (pas d'écran de vérification du numéro de sécurité :
faites-la depuis le téléphone).

## Développement

```sh
cargo test      # modèle de messages et interface (terminal simulé)
cargo run
```

- `src/model.rs` : reconstruction des conversations à partir des contenus stockés par presage ;
- `src/signal.rs` : réception, envoi, pièces jointes ;
- `src/ui.rs` : interface ;
- `src/sidecar.rs` : état de lecture et de distribution ;
- `src/main.rs` : ligne de commande, association.

## Licence

presage est sous licence AGPL-3.0 : toute redistribution de tuisignal doit l'être aussi.
