//! Modèle d'affichage : transforme les `Content` stockés par presage en messages lisibles.

use std::path::{Path, PathBuf};

use presage::libsignal_service::content::{Content, ContentBody};
use presage::libsignal_service::prelude::Uuid;
use presage::proto::sync_message::Content as SyncContent;
use presage::proto::{AttachmentPointer, DataMessage, EditMessage, SyncMessage};
use presage::store::{ContentExt, Thread};

pub const DELETED: &str = "Message supprimé";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Sending,
    Failed,
    Sent,
    Delivered,
    Read,
}

#[derive(Clone, Debug)]
pub struct Att {
    pub name: String,
    pub mime: String,
    pub size: u64,
    pub path: PathBuf,
    /// Présent pour les pièces jointes téléchargeables depuis les serveurs.
    pub pointer: Option<AttachmentPointer>,
}

impl Att {
    pub fn is_image(&self) -> bool {
        self.mime.starts_with("image/")
    }
}

#[derive(Clone, Debug)]
pub struct Msg {
    pub ts: u64,
    /// Horodatages des révisions (message modifié) : réactions et accusés peuvent les viser.
    pub alt_ts: Vec<u64>,
    pub sender: Uuid,
    pub outgoing: bool,
    pub body: String,
    pub atts: Vec<Att>,
    pub reactions: Vec<(Uuid, String)>,
    pub edited: bool,
    pub deleted: bool,
    pub status: Status,
}

impl Msg {
    fn matches(&self, ts: u64) -> bool {
        self.ts == ts || self.alt_ts.contains(&ts)
    }

    pub fn preview(&self) -> String {
        if self.body.is_empty() && !self.atts.is_empty() {
            format!("📎 {}", self.atts[0].name)
        } else {
            self.body.replace('\n', " ")
        }
    }
}

pub fn thread_key(thread: &Thread) -> String {
    match thread {
        Thread::Contact(id) => format!("c:{}", id.raw_uuid()),
        Thread::Group(key) => format!("g:{}", hex::encode(key)),
    }
}

/// Nom de fichier local, déterministe, d'une pièce jointe.
pub fn att_path(dir: &Path, ts: u64, idx: usize, pointer: &AttachmentPointer) -> PathBuf {
    dir.join(format!("{ts}-{idx}-{}", att_name(pointer)))
}

pub fn att_name(pointer: &AttachmentPointer) -> String {
    let clean: String = pointer
        .file_name
        .as_deref()
        .unwrap_or("")
        .chars()
        .map(|c| if c == '/' || c == '\0' || c.is_control() { '_' } else { c })
        .collect();
    let clean = clean.trim_matches(['.', ' ']).to_string();
    if !clean.is_empty() {
        return clean;
    }
    let mime = pointer.content_type.as_deref().unwrap_or("application/octet-stream");
    let ext = mime_guess::get_mime_extensions_str(mime)
        .and_then(|e| {
            // « jpe » est le premier candidat pour image/jpeg : on préfère jpg.
            e.iter().find(|x| ["jpg", "png", "mp4", "pdf", "txt"].contains(x)).or(e.first())
        })
        .copied()
        .unwrap_or("bin");
    format!("fichier.{ext}")
}

fn atts_of(dm: &DataMessage, ts: u64, dir: &Path) -> Vec<Att> {
    dm.attachments
        .iter()
        .enumerate()
        .map(|(idx, p)| Att {
            name: att_name(p),
            mime: p.content_type.clone().unwrap_or_else(|| "application/octet-stream".into()),
            size: p.size.unwrap_or(0) as u64,
            path: att_path(dir, ts, idx, p),
            pointer: Some(p.clone()),
        })
        .collect()
}

/// Pièces jointes à télécharger pour un contenu reçu : (pointeur, chemin local).
pub fn downloads_of(content: &Content, dir: &Path) -> Vec<(AttachmentPointer, PathBuf)> {
    let dm = match &content.body {
        ContentBody::DataMessage(dm) => dm,
        ContentBody::SynchronizeMessage(SyncMessage {
            content: Some(SyncContent::Sent(sent)), ..
        }) => match &sent.message {
            Some(dm) => dm,
            None => return vec![],
        },
        _ => return vec![],
    };
    let ts = content.timestamp();
    dm.attachments
        .iter()
        .enumerate()
        .map(|(idx, p)| (p.clone(), att_path(dir, ts, idx, p)))
        .collect()
}

fn body_of(dm: &DataMessage) -> String {
    let mut body = dm.body.clone().unwrap_or_default();
    if dm.sticker.is_some() && body.is_empty() {
        body = "[autocollant]".into();
    }
    if let Some(text) = dm.quote.as_ref().and_then(|q| q.text.as_deref()) {
        let quoted: String = text.replace('\n', " ").chars().take(120).collect();
        body = format!("> {quoted}\n{body}");
    }
    body
}

fn apply_data(msgs: &mut Vec<Msg>, sender: Uuid, outgoing: bool, ts: u64, dm: &DataMessage, dir: &Path) {
    if let Some(reaction) = &dm.reaction {
        let Some(target) = reaction.target_sent_timestamp else { return };
        if let Some(msg) = msgs.iter_mut().find(|m| m.matches(target)) {
            msg.reactions.retain(|(who, _)| *who != sender);
            if !reaction.remove.unwrap_or(false) {
                msg.reactions.push((sender, reaction.emoji.clone().unwrap_or_default()));
            }
        }
        return;
    }
    if let Some(target) = dm.delete.as_ref().and_then(|d| d.target_sent_timestamp) {
        if let Some(msg) = msgs.iter_mut().find(|m| m.matches(target) && m.sender == sender) {
            mark_deleted(msg);
        }
        return;
    }
    let body = body_of(dm);
    let atts = atts_of(dm, ts, dir);
    if body.is_empty() && atts.is_empty() {
        return; // mise à jour de groupe, minuteur, clé de profil…
    }
    if msgs.iter().any(|m| m.ts == ts && m.sender == sender) {
        return;
    }
    msgs.push(Msg {
        ts,
        alt_ts: vec![],
        sender,
        outgoing,
        body,
        atts,
        reactions: vec![],
        edited: false,
        deleted: false,
        status: Status::Sent,
    });
}

fn mark_deleted(msg: &mut Msg) {
    msg.body = DELETED.into();
    msg.atts.clear();
    msg.reactions.clear();
    msg.deleted = true;
}

fn apply_edit(msgs: &mut [Msg], ts: u64, edit: &EditMessage) {
    let (Some(target), Some(dm)) = (edit.target_sent_timestamp, &edit.data_message) else { return };
    if let Some(msg) = msgs.iter_mut().find(|m| m.matches(target)) {
        msg.body = body_of(dm);
        msg.edited = true;
        msg.alt_ts.push(dm.timestamp.unwrap_or(ts));
        if !msg.alt_ts.contains(&ts) {
            msg.alt_ts.push(ts);
        }
    }
}

/// Reconstruit la conversation à partir des contenus, donnés dans l'ordre chronologique.
pub fn fold<'a>(contents: impl IntoIterator<Item = &'a Content>, me: Uuid, dir: &Path) -> Vec<Msg> {
    let mut msgs: Vec<Msg> = Vec::new();
    for content in contents {
        let sender = content.metadata.sender.raw_uuid();
        let ts = content.timestamp();
        match &content.body {
            // presage remplace un message supprimé par un NullMessage d'expéditeur nul.
            ContentBody::NullMessage(_) if sender.is_nil() => {
                let mut msg = Msg {
                    ts,
                    alt_ts: vec![],
                    sender,
                    outgoing: false,
                    body: String::new(),
                    atts: vec![],
                    reactions: vec![],
                    edited: false,
                    deleted: true,
                    status: Status::Sent,
                };
                mark_deleted(&mut msg);
                msgs.push(msg);
            }
            ContentBody::DataMessage(dm) => apply_data(&mut msgs, sender, sender == me, ts, dm, dir),
            ContentBody::SynchronizeMessage(SyncMessage {
                content: Some(SyncContent::Sent(sent)), ..
            }) => {
                if let Some(dm) = &sent.message {
                    apply_data(&mut msgs, me, true, ts, dm, dir);
                } else if let Some(edit) = &sent.edit_message {
                    apply_edit(&mut msgs, ts, edit);
                }
            }
            ContentBody::EditMessage(edit) => apply_edit(&mut msgs, ts, edit),
            _ => {}
        }
    }
    msgs.sort_by_key(|m| m.ts);
    msgs
}

/// Nature d'un contenu reçu en direct, pour la liste des conversations et les notifications.
pub enum Incoming {
    /// Nouveau message visible : (sortant, aperçu).
    Message { outgoing: bool, preview: String },
    /// Réaction, modification, suppression : la conversation doit être réaffichée.
    Update,
    /// Accusé de réception ou de lecture.
    Receipt { read: bool, timestamps: Vec<u64> },
    Other,
}

pub fn classify(content: &Content, me: Uuid) -> Incoming {
    let data = |dm: &DataMessage, outgoing: bool| {
        if dm.reaction.is_some() || dm.delete.is_some() {
            return Incoming::Update;
        }
        let body = body_of(dm);
        if body.is_empty() && dm.attachments.is_empty() {
            return Incoming::Other;
        }
        let preview = if body.is_empty() {
            format!("📎 {}", att_name(&dm.attachments[0]))
        } else {
            body.replace('\n', " ")
        };
        Incoming::Message { outgoing, preview }
    };
    match &content.body {
        ContentBody::DataMessage(dm) => data(dm, content.metadata.sender.raw_uuid() == me),
        ContentBody::SynchronizeMessage(SyncMessage {
            content: Some(SyncContent::Sent(sent)), ..
        }) => match (&sent.message, &sent.edit_message) {
            (Some(dm), _) => data(dm, true),
            (None, Some(_)) => Incoming::Update,
            _ => Incoming::Other,
        },
        ContentBody::EditMessage(_) => Incoming::Update,
        ContentBody::ReceiptMessage(receipt) => {
            use presage::proto::receipt_message::Type;
            match receipt.r#type() {
                Type::Delivery => Incoming::Receipt { read: false, timestamps: receipt.timestamp.clone() },
                Type::Read | Type::Viewed => {
                    Incoming::Receipt { read: true, timestamps: receipt.timestamp.clone() }
                }
            }
        }
        _ => Incoming::Other,
    }
}

pub fn human_size(n: u64) -> String {
    match n {
        0..1024 => format!("{n} o"),
        1024..1_048_576 => format!("{} ko", n / 1024),
        _ => format!("{:.1} Mo", n as f64 / 1_048_576.0),
    }
}

/// Normalise un chemin saisi ou collé (guillemets, file://, espaces échappés, ~).
pub fn clean_path(raw: &str) -> PathBuf {
    let mut s = raw.trim();
    if s.len() >= 2 && (s.starts_with('\'') && s.ends_with('\'') || s.starts_with('"') && s.ends_with('"')) {
        s = &s[1..s.len() - 1];
    }
    let mut s = s.replace("\\ ", " ");
    if let Some(rest) = s.strip_prefix("file://") {
        s = percent_decode(rest);
    }
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(s)
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 3 <= bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
            if let Ok(b) = u8::from_str_radix(hex, 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use presage::libsignal_service::content::Metadata;
    use presage::libsignal_service::protocol::{Aci, ServiceId};
    use presage::proto::data_message::{Delete, Quote, Reaction};
    use presage::proto::sync_message::Sent;
    use presage::proto::{NullMessage, ReceiptMessage};

    fn uuid(n: u8) -> Uuid {
        Uuid::from_bytes([n; 16])
    }

    fn content(sender: Uuid, ts: u64, body: impl Into<ContentBody>) -> Content {
        let time = Utc.timestamp_millis_opt(ts as i64).unwrap();
        Content {
            metadata: Metadata {
                sender: ServiceId::Aci(Aci::from(sender)),
                destination: ServiceId::Aci(Aci::from(uuid(9))),
                sender_device: 1u32.try_into().unwrap(),
                pni_verified: None,
                client_timestamp: time,
                server_timestamp: time,
                needs_receipt: false,
                unidentified_sender: false,
                was_plaintext: false,
                server_guid: None,
            },
            body: body.into(),
        }
    }

    fn text(ts: u64, body: &str) -> DataMessage {
        DataMessage { body: Some(body.into()), timestamp: Some(ts), ..Default::default() }
    }

    #[test]
    fn folds_a_conversation() {
        let (me, alice) = (uuid(9), uuid(1));
        let dir = Path::new("/att");
        let photo = AttachmentPointer {
            content_type: Some("image/jpeg".into()),
            size: Some(2048),
            ..Default::default()
        };
        let contents = vec![
            content(alice, 10, text(10, "salut")),
            content(me, 20, text(20, "ça va ?")),
            // réaction d'Alice sur mon message, puis changement d'emoji
            content(alice, 30, DataMessage {
                reaction: Some(Reaction { emoji: Some("👍".into()), target_sent_timestamp: Some(20), ..Default::default() }),
                ..Default::default()
            }),
            content(alice, 31, DataMessage {
                reaction: Some(Reaction { emoji: Some("❤️".into()), target_sent_timestamp: Some(20), ..Default::default() }),
                ..Default::default()
            }),
            // image avec citation
            content(alice, 40, DataMessage {
                body: Some("regarde".into()),
                timestamp: Some(40),
                attachments: vec![photo],
                quote: Some(Quote { text: Some("ça va ?".into()), ..Default::default() }),
                ..Default::default()
            }),
            // message envoyé depuis le téléphone, puis modifié
            content(me, 50, SyncMessage {
                content: Some(SyncContent::Sent(Sent { timestamp: Some(50), message: Some(text(50, "depuis le tel")), ..Default::default() })),
                ..Default::default()
            }),
            content(me, 60, SyncMessage {
                content: Some(SyncContent::Sent(Sent {
                    timestamp: Some(60),
                    edit_message: Some(EditMessage { target_sent_timestamp: Some(50), data_message: Some(text(60, "depuis le téléphone")) }),
                    ..Default::default()
                })),
                ..Default::default()
            }),
            // message supprimé (tel que presage le stocke), bourrage, accusé : ignorés ou marqués
            content(Uuid::nil(), 70, NullMessage::default()),
            content(alice, 80, NullMessage::default()),
            content(alice, 90, ReceiptMessage { r#type: Some(1), timestamp: vec![20] }),
            // suppression en direct d'un message encore présent
            content(alice, 100, text(100, "oups")),
            content(alice, 110, DataMessage { delete: Some(Delete { target_sent_timestamp: Some(100) }), ..Default::default() }),
        ];
        let msgs = fold(&contents, me, dir);
        let bodies: Vec<_> = msgs.iter().map(|m| (m.ts, m.outgoing, m.body.as_str())).collect();
        assert_eq!(bodies, vec![
            (10, false, "salut"),
            (20, true, "ça va ?"),
            (40, false, "> ça va ?\nregarde"),
            (50, true, "depuis le téléphone"),
            (70, false, DELETED),
            (100, false, DELETED),
        ]);
        assert_eq!(msgs[1].reactions, vec![(alice, "❤️".to_string())]);
        assert!(msgs[3].edited && msgs[3].alt_ts.contains(&60));
        let att = &msgs[2].atts[0];
        assert!(att.is_image());
        assert_eq!(att.path, Path::new("/att/40-0-fichier.jpg"));
        assert_eq!(downloads_of(&contents[4], dir)[0].1, att.path);
    }

    #[test]
    fn classifies_incoming() {
        let (me, alice) = (uuid(9), uuid(1));
        assert!(matches!(
            classify(&content(alice, 1, text(1, "a\nb")), me),
            Incoming::Message { outgoing: false, preview } if preview == "a b"
        ));
        assert!(matches!(classify(&content(me, 1, text(1, "x")), me), Incoming::Message { outgoing: true, .. }));
        assert!(matches!(
            classify(&content(alice, 1, ReceiptMessage { r#type: Some(1), timestamp: vec![5] }), me),
            Incoming::Receipt { read: true, timestamps } if timestamps == [5]
        ));
        assert!(matches!(classify(&content(alice, 1, DataMessage::default()), me), Incoming::Other));
    }

    #[test]
    fn cleans_paths_and_names() {
        assert_eq!(clean_path("'/tmp/mon fichier.txt'"), Path::new("/tmp/mon fichier.txt"));
        assert_eq!(clean_path("file:///tmp/a%20b.png"), Path::new("/tmp/a b.png"));
        assert_eq!(clean_path("/tmp/a\\ b"), Path::new("/tmp/a b"));
        let evil = AttachmentPointer { file_name: Some("../../etc/passwd".into()), ..Default::default() };
        assert_eq!(att_name(&evil), "_.._etc_passwd");
        assert_eq!(human_size(2048), "2 ko");
    }
}
