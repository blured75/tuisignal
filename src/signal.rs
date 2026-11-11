//! Opérations réseau Signal (via presage / libsignal), exécutées en tâches locales.

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, anyhow};
use futures::{StreamExt, pin_mut};
use image::DynamicImage;
use presage::Manager;
use presage::libsignal_service::content::ContentBody;
use presage::libsignal_service::prelude::Uuid;
use presage::libsignal_service::protocol::{Aci, ServiceId};
use presage::libsignal_service::sender::AttachmentSpec;
use presage::manager::Registered;
use presage::model::messages::Received;
use presage::proto::{AttachmentPointer, DataMessage, GroupContextV2, ReceiptMessage, receipt_message};
use presage::store::Thread;
use presage_store_sqlite::SqliteStore;
use tokio::sync::mpsc::UnboundedSender;

use crate::model;

pub type Mgr = Manager<SqliteStore, Registered>;

/// Au-delà, la pièce jointe n'est téléchargée qu'à la demande (Ctrl+P).
pub const AUTO_DOWNLOAD_MAX: u32 = 50 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub enum Conn {
    Connecting,
    Syncing,
    Ready,
    Lost(String),
}

pub enum Ev {
    Conn(Conn),
    Received(Received),
    Downloaded { path: PathBuf, error: Option<String>, open: bool },
    SendDone { key: String, ts: u64, error: Option<String> },
    Image { path: PathBuf, image: Option<DynamicImage> },
    Toast { text: String, error: bool },
}

pub type Tx = UnboundedSender<Ev>;

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// Reçoit les messages en continu et se reconnecte en cas de coupure.
pub async fn receive_loop(manager: Mgr, tx: Tx, att_dir: PathBuf) {
    let mut delay = 2;
    loop {
        let _ = tx.send(Ev::Conn(Conn::Connecting));
        let mut receiver = manager.clone();
        match receiver.receive_messages().await {
            Ok(stream) => {
                let _ = tx.send(Ev::Conn(Conn::Syncing));
                pin_mut!(stream);
                while let Some(received) = stream.next().await {
                    delay = 2;
                    if let Received::Content(content) = &received {
                        for (pointer, path) in model::downloads_of(content, &att_dir) {
                            if pointer.size.unwrap_or(0) <= AUTO_DOWNLOAD_MAX && !path.exists() {
                                tokio::task::spawn_local(download(
                                    manager.clone(), tx.clone(), pointer, path, false,
                                ));
                            }
                        }
                    }
                    if tx.send(Ev::Received(received)).is_err() {
                        return;
                    }
                }
                let _ = tx.send(Ev::Conn(Conn::Lost("connexion fermée".into())));
            }
            Err(error) => {
                let _ = tx.send(Ev::Conn(Conn::Lost(error.to_string())));
            }
        }
        tokio::time::sleep(Duration::from_secs(delay)).await;
        delay = (delay * 2).min(60);
    }
}

pub async fn download(manager: Mgr, tx: Tx, pointer: AttachmentPointer, path: PathBuf, open: bool) {
    let result = async {
        let data = manager.get_attachment(&pointer).await.map_err(|e| anyhow!("{e}"))?;
        let tmp = path.with_extension("part");
        tokio::fs::write(&tmp, &data).await?;
        tokio::fs::rename(&tmp, &path).await?;
        anyhow::Ok(())
    }
    .await;
    let _ = tx.send(Ev::Downloaded { path, error: result.err().map(|e| e.to_string()), open });
}

/// Envoie un message texte et/ou des fichiers. `files` : (chemin local, nom affiché).
pub async fn send(
    mut manager: Mgr,
    thread: Thread,
    text: String,
    files: Vec<(PathBuf, String)>,
    ts: u64,
) -> anyhow::Result<()> {
    let mut specs = Vec::new();
    for (path, name) in files {
        let data = tokio::fs::read(&path)
            .await
            .with_context(|| format!("lecture de {}", path.display()))?;
        let content_type = mime_guess::from_path(&name).first_or_octet_stream().to_string();
        let (width, height) = if content_type.starts_with("image/") {
            image::image_dimensions(&path).map(|(w, h)| (Some(w), Some(h))).unwrap_or((None, None))
        } else {
            (None, None)
        };
        specs.push((
            AttachmentSpec {
                content_type,
                length: data.len(),
                file_name: Some(name),
                preview: None,
                voice_note: None,
                borderless: None,
                width,
                height,
                caption: None,
                blur_hash: None,
            },
            data,
        ));
    }
    let mut attachments = Vec::new();
    for uploaded in manager.upload_attachments(specs).await.map_err(|e| anyhow!("{e}"))? {
        attachments.push(uploaded.map_err(|e| anyhow!("envoi de la pièce jointe : {e}"))?);
    }
    let mut message = DataMessage {
        body: (!text.is_empty()).then_some(text),
        attachments,
        timestamp: Some(ts),
        ..Default::default()
    };
    match thread {
        Thread::Contact(recipient) => {
            manager.send_message(recipient, message, ts).await.map_err(|e| anyhow!("{e}"))?;
        }
        Thread::Group(key) => {
            message.group_v2 = Some(GroupContextV2 {
                master_key: Some(key.to_vec()),
                revision: Some(0),
                ..Default::default()
            });
            manager.send_message_to_group(&key, message, ts).await.map_err(|e| anyhow!("{e}"))?;
        }
    }
    Ok(())
}

/// Accusés de lecture, groupés par expéditeur.
pub async fn send_read_receipts(mut manager: Mgr, by_sender: Vec<(Uuid, Vec<u64>)>) {
    for (sender, timestamps) in by_sender {
        let receipt = ReceiptMessage {
            r#type: Some(receipt_message::Type::Read as i32),
            timestamp: timestamps,
        };
        let recipient = ServiceId::Aci(Aci::from(sender));
        let body: ContentBody = receipt.into();
        if let Err(error) = manager.send_message(recipient, body, now_ms()).await {
            tracing::warn!(%error, "accusé de lecture non envoyé");
        }
    }
}

pub async fn request_contacts(mut manager: Mgr, tx: Tx) {
    if let Err(error) = manager.request_contacts().await {
        let _ = tx.send(Ev::Toast { text: format!("Synchronisation impossible : {error}"), error: true });
    }
}

/// Décode et réduit une image hors du fil d'interface.
pub fn decode_image(path: PathBuf, tx: Tx) {
    tokio::task::spawn_blocking(move || {
        let image = image::ImageReader::open(&path)
            .ok()
            .and_then(|r| r.with_guessed_format().ok())
            .and_then(|r| r.decode().ok())
            .map(|img| img.thumbnail(800, 800));
        let _ = tx.send(Ev::Image { path, image });
    });
}
