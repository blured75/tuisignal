//! tuisignal — client Signal en mode console, bâti sur libsignal (via presage).

mod model;
mod sidecar;
mod signal;
mod ui;

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow, bail};
use clap::{Parser, Subcommand};
use directories::ProjectDirs;
use futures::channel::oneshot;
use futures::future;
use presage::Manager;
use presage::libsignal_service::configuration::SignalServers;
use presage::model::identity::OnNewIdentity;
use presage_store_sqlite::SqliteStore;

#[derive(Parser)]
#[command(name = "tuisignal", version, about = "Client Signal en mode console")]
struct Args {
    #[command(subcommand)]
    command: Option<Cmd>,
    /// Ne pas envoyer d'accusés de lecture
    #[arg(long)]
    no_read_receipts: bool,
    /// Notifications de bureau (notify-send) pour les messages reçus
    #[arg(long)]
    notify: bool,
    /// Dossier des données (par défaut ~/.local/share/tuisignal)
    #[arg(long, global = true, value_name = "DOSSIER")]
    data_dir: Option<PathBuf>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Associer cet ordinateur à votre compte Signal (QR code à scanner avec le téléphone)
    Link {
        /// Nom de l'appareil affiché dans Signal
        #[arg(short, long, default_value = "tuisignal")]
        name: String,
    },
    /// Dissocier cet ordinateur du compte et effacer les données locales
    Unlink,
}

struct Paths {
    data: PathBuf,
    db: PathBuf,
    attachments: PathBuf,
    sidecar: PathBuf,
    log: PathBuf,
}

fn paths(data_dir: Option<PathBuf>) -> anyhow::Result<Paths> {
    let dirs = ProjectDirs::from("", "", "tuisignal").ok_or_else(|| anyhow!("dossier personnel introuvable"))?;
    let data = data_dir.unwrap_or_else(|| dirs.data_dir().to_path_buf());
    let state = dirs.state_dir().unwrap_or(dirs.data_dir()).to_path_buf();
    for dir in [&data, &data.join("attachments"), &state] {
        std::fs::create_dir_all(dir).with_context(|| format!("création de {}", dir.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
        }
    }
    Ok(Paths {
        db: data.join("signal.db"),
        attachments: data.join("attachments"),
        sidecar: data.join("state.json"),
        log: state.join("tuisignal.log"),
        data,
    })
}

/// Une base SQLite en clair commence par cet en-tête ; chiffrée (SQLCipher), non.
fn is_encrypted(db: &Path) -> bool {
    let mut header = [0u8; 16];
    match std::fs::File::open(db).and_then(|mut f| f.read_exact(&mut header)) {
        Ok(()) => &header != b"SQLite format 3\0",
        Err(_) => false,
    }
}

fn ask_passphrase(prompt: &str) -> anyhow::Result<String> {
    if let Ok(pass) = std::env::var("TUISIGNAL_PASSPHRASE") {
        return Ok(pass);
    }
    Ok(rpassword::prompt_password(prompt)?)
}

async fn open_store(db: &Path, passphrase: Option<&str>) -> anyhow::Result<SqliteStore> {
    let url = format!("sqlite://{}?mode=rwc", db.display());
    SqliteStore::open_with_passphrase(&url, passphrase, OnNewIdentity::Trust)
        .await
        .map_err(|e| anyhow!("ouverture de la base {} : {e}", db.display()))
}

async fn open_existing(paths: &Paths) -> anyhow::Result<SqliteStore> {
    if !paths.db.exists() {
        bail!("Aucun compte Signal associé. Lancez d'abord : tuisignal link");
    }
    if is_encrypted(&paths.db) {
        let pass = ask_passphrase("Phrase de passe : ")?;
        open_store(&paths.db, Some(&pass)).await.context("phrase de passe incorrecte ?")
    } else {
        open_store(&paths.db, None).await
    }
}

async fn link(paths: &Paths, name: String) -> anyhow::Result<()> {
    if paths.db.exists() {
        bail!(
            "Un compte est déjà associé ({}).\nPour recommencer : tuisignal unlink",
            paths.db.display()
        );
    }
    println!("La base locale (clés, contacts, messages) peut être chiffrée par une phrase de passe,");
    println!("demandée à chaque lancement. Laissez vide pour ne pas chiffrer.");
    let pass = ask_passphrase("Phrase de passe : ")?;
    if !pass.is_empty() && std::env::var("TUISIGNAL_PASSPHRASE").is_err() {
        if rpassword::prompt_password("Confirmez : ")? != pass {
            bail!("Les deux saisies ne correspondent pas.");
        }
    }
    let store = open_store(&paths.db, (!pass.is_empty()).then_some(pass.as_str())).await?;

    println!("\nConnexion à Signal…");
    let (tx, rx) = oneshot::channel();
    let (manager, _) = future::join(
        Manager::link_secondary_device(store, SignalServers::Production, name, tx),
        async move {
            if let Ok(url) = rx.await {
                let _ = qr2term::print_qr(url.to_string());
                println!("\n{url}\n");
                println!("Sur votre téléphone : Signal → Paramètres → Appareils associés →");
                println!("Associer un nouvel appareil, puis scannez ce QR code.\nEn attente du scan…");
            }
        },
    )
    .await;
    match manager {
        Ok(manager) => {
            let ids = &manager.registration_data().service_ids;
            println!("\nAppareil associé (compte {}).\nLancez maintenant : tuisignal", ids.aci);
            Ok(())
        }
        Err(error) => {
            // Rien d'utilisable n'a été enregistré : on repart de zéro au prochain essai.
            remove_db(&paths.db);
            Err(anyhow!("association échouée : {error}"))
        }
    }
}

fn remove_db(db: &Path) {
    for suffix in ["", "-wal", "-shm"] {
        let mut name = db.as_os_str().to_owned();
        name.push(suffix);
        let _ = std::fs::remove_file(PathBuf::from(name));
    }
}

async fn unlink(paths: &Paths) -> anyhow::Result<()> {
    let store = open_existing(paths).await?;
    match Manager::load_registered(store).await {
        Ok(manager) => match manager.unlink_self().await {
            Ok(()) => println!("Appareil dissocié du compte Signal."),
            Err(error) => println!(
                "Dissociation côté serveur impossible ({error}).\n\
                 Retirez l'appareil depuis le téléphone : Paramètres → Appareils associés."
            ),
        },
        Err(error) => println!("Compte illisible ({error}) ; suppression des données locales."),
    }
    remove_db(&paths.db);
    let _ = std::fs::remove_file(&paths.sidecar);
    let _ = std::fs::remove_dir_all(&paths.attachments);
    println!("Données locales effacées ({}).", paths.data.display());
    Ok(())
}

fn init_log(path: &Path) {
    use tracing_subscriber::EnvFilter;
    let Ok(file) = std::fs::OpenOptions::new().create(true).append(true).open(path) else { return };
    let filter = EnvFilter::try_from_env("TUISIGNAL_LOG").unwrap_or_else(|_| EnvFilter::new("warn"));
    let _ = tracing_subscriber::fmt()
        .with_writer(std::sync::Mutex::new(file))
        .with_ansi(false)
        .with_env_filter(filter)
        .try_init();
}

async fn run(args: Args) -> anyhow::Result<()> {
    let paths = paths(args.data_dir)?;
    match args.command {
        Some(Cmd::Link { name }) => link(&paths, name).await,
        Some(Cmd::Unlink) => unlink(&paths).await,
        None => {
            let store = open_existing(&paths).await?;
            let manager = Manager::load_registered(store)
                .await
                .map_err(|e| anyhow!("compte inutilisable ({e}). Essayez : tuisignal unlink puis tuisignal link"))?;
            init_log(&paths.log);
            let sidecar = sidecar::Sidecar::load(paths.sidecar);
            let opts = ui::Options { read_receipts: !args.no_read_receipts, notify: args.notify };
            ui::run(manager, paths.attachments, sidecar, opts).await
        }
    }
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let args = Args::parse();
    // presage n'est pas `Send` : tout tourne dans un LocalSet, sur ce fil.
    let local = tokio::task::LocalSet::new();
    if let Err(error) = local.run_until(run(args)).await {
        eprintln!("Erreur : {error:#}");
        std::process::exit(1);
    }
    // Les tâches de fond (réception) n'ont pas de fin propre.
    std::process::exit(0);
}
