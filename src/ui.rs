//! Interface ratatui : liste des conversations, messages, saisie, fenêtres modales.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use chrono::{Datelike, Local, TimeZone};
use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, Event, EventStream, KeyCode, KeyEvent,
    KeyEventKind, KeyModifiers,
};
use futures::StreamExt;
use presage::libsignal_service::prelude::Uuid;
use presage::libsignal_service::protocol::{Aci, ServiceId};
use presage::model::messages::Received;
use presage::proto::AttachmentPointer;
use presage::store::{ContentExt, ContentsStore, Thread};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect, Size};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui_image::picker::Picker;
use ratatui_image::protocol::Protocol;
use ratatui_image::{Image, Resize};
use tokio::sync::mpsc::unbounded_channel;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::model::{self, Att, Incoming, Msg, Status, att_name, att_path, clean_path, human_size, thread_key};
use crate::sidecar::Sidecar;
use crate::signal::{self, Conn, Ev, Mgr, Tx, now_ms};

const SIDE_WIDTH: u16 = 34;
const IMG_COLS: u16 = 40;
const IMG_ROWS: u16 = 12;
const MAX_CONTENTS: usize = 3000;
const NAME_COLORS: [Color; 8] = [
    Color::Cyan, Color::Magenta, Color::Yellow, Color::Blue, Color::LightRed, Color::LightCyan,
    Color::LightMagenta, Color::LightBlue,
];
const DAYS: [&str; 7] = ["lundi", "mardi", "mercredi", "jeudi", "vendredi", "samedi", "dimanche"];
const MONTHS: [&str; 12] = [
    "janvier", "février", "mars", "avril", "mai", "juin", "juillet", "août", "septembre",
    "octobre", "novembre", "décembre",
];

pub struct Options {
    pub read_receipts: bool,
    pub notify: bool,
}

#[derive(Clone)]
pub struct Chat {
    pub key: String,
    pub thread: Thread,
    pub name: String,
    pub sub: String,
    pub last_ts: u64,
    pub unread: u32,
    pub preview: String,
}

#[derive(PartialEq, Clone, Copy)]
enum Focus {
    Side,
    Chat,
}

enum ImgState {
    Loading,
    Ready(Protocol),
    Failed,
}

enum Row {
    Text(Line<'static>),
    Image { path: PathBuf, part: u16 },
}

#[derive(Default)]
struct Input {
    chars: Vec<char>,
    cur: usize,
    off: usize,
}

impl Input {
    fn text(&self) -> String {
        self.chars.iter().collect()
    }

    fn take(&mut self) -> String {
        let text = self.text();
        *self = Input::default();
        text
    }

    #[cfg(test)]
    fn set(&mut self, text: &str) {
        self.chars = text.chars().collect();
        self.cur = self.chars.len();
        self.off = 0;
    }

    fn insert_str(&mut self, text: &str) {
        for c in text.chars().filter(|c| *c == '\n' || !c.is_control()) {
            self.chars.insert(self.cur, c);
            self.cur += 1;
        }
    }

    fn key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Char('a') if ctrl => self.cur = 0,
            KeyCode::Char('e') if ctrl => self.cur = self.chars.len(),
            KeyCode::Char('u') if ctrl => {
                self.chars.drain(..self.cur);
                self.cur = 0;
            }
            KeyCode::Char('k') if ctrl => self.chars.truncate(self.cur),
            KeyCode::Char('w') if ctrl => self.delete_word(),
            KeyCode::Backspace if ctrl || alt => self.delete_word(),
            KeyCode::Enter if alt => self.insert_str("\n"),
            KeyCode::Char(c) if !ctrl => self.insert_str(c.encode_utf8(&mut [0; 4])),
            KeyCode::Backspace if self.cur > 0 => {
                self.cur -= 1;
                self.chars.remove(self.cur);
            }
            KeyCode::Delete if self.cur < self.chars.len() => {
                self.chars.remove(self.cur);
            }
            KeyCode::Left => self.cur = self.cur.saturating_sub(1),
            KeyCode::Right => self.cur = (self.cur + 1).min(self.chars.len()),
            KeyCode::Home => self.cur = 0,
            KeyCode::End => self.cur = self.chars.len(),
            _ => {}
        }
    }

    fn delete_word(&mut self) {
        let mut start = self.cur;
        while start > 0 && self.chars[start - 1].is_whitespace() {
            start -= 1;
        }
        while start > 0 && !self.chars[start - 1].is_whitespace() {
            start -= 1;
        }
        self.chars.drain(start..self.cur);
        self.cur = start;
    }

    /// Texte visible et colonne du curseur pour une largeur donnée.
    fn view(&mut self, width: usize) -> (String, u16) {
        let shown = |c: &char| if *c == '\n' { '⏎' } else { *c };
        let w = |c: &char| shown(c).width().unwrap_or(0);
        self.off = self.off.min(self.cur);
        while self.chars[self.off..self.cur].iter().map(w).sum::<usize>() >= width.max(1) {
            self.off += 1;
        }
        let mut out = String::new();
        let mut used = 0;
        for c in &self.chars[self.off..] {
            used += w(c);
            if used > width {
                break;
            }
            out.push(shown(c));
        }
        let cursor = self.chars[self.off..self.cur].iter().map(w).sum::<usize>();
        (out, cursor as u16)
    }
}

struct FilePicker {
    dir: PathBuf,
    entries: Vec<(String, bool)>,
    filter: String,
    sel: usize,
}

impl FilePicker {
    fn new(dir: PathBuf) -> Self {
        let mut picker = FilePicker { dir, entries: vec![], filter: String::new(), sel: 0 };
        picker.read();
        picker
    }

    fn read(&mut self) {
        let mut entries: Vec<(String, bool)> = std::fs::read_dir(&self.dir)
            .map(|rd| {
                rd.flatten()
                    .map(|e| {
                        let is_dir = e.path().is_dir();
                        (e.file_name().to_string_lossy().into_owned(), is_dir)
                    })
                    .collect()
            })
            .unwrap_or_default();
        entries.sort_by_key(|(name, is_dir)| (!is_dir, name.to_lowercase()));
        self.entries = entries;
        self.filter.clear();
        self.sel = 0;
    }

    fn visible(&self) -> Vec<&(String, bool)> {
        let needle = self.filter.to_lowercase();
        self.entries
            .iter()
            .filter(|(name, _)| {
                if needle.is_empty() {
                    !name.starts_with('.')
                } else {
                    name.to_lowercase().contains(&needle)
                }
            })
            .collect()
    }

    fn enter(&mut self, dir: PathBuf) {
        self.dir = dir;
        self.read();
    }

    fn parent(&mut self) {
        if let Some(parent) = self.dir.parent() {
            self.enter(parent.to_path_buf());
        }
    }
}

struct AttList {
    items: Vec<(Att, String)>,
    sel: usize,
}

enum Modal {
    Files(FilePicker),
    Atts(AttList),
}

pub struct App {
    mgr: Option<Mgr>,
    me: Uuid,
    tx: Tx,
    sidecar: Sidecar,
    att_dir: PathBuf,
    opts: Options,
    names: HashMap<Uuid, String>,
    unknown: HashSet<Uuid>,
    names_dirty: Option<Instant>,
    previews: HashMap<String, String>,
    pub chats: Vec<Chat>,
    filter: String,
    sel: usize,
    side_off: usize,
    pub current: Option<String>,
    pub msgs: Vec<Msg>,
    pending: Vec<(String, Msg)>,
    scroll: usize,
    input: Input,
    files: Vec<PathBuf>,
    last_dir: PathBuf,
    focus: Focus,
    modal: Option<Modal>,
    conn: Conn,
    ready: bool,
    asked_contacts: bool,
    toast: Option<(String, bool, Instant)>,
    picker: Picker,
    images: HashMap<PathBuf, ImgState>,
    quit: bool,
}

fn short(uuid: &Uuid) -> String {
    uuid.to_string()[..8].to_string()
}

fn fit(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars() {
        let w = c.width().unwrap_or(0);
        if used + w + 1 > width {
            break;
        }
        used += w;
        out.push(c);
    }
    out.push('…');
    out
}

fn local(ts: u64) -> chrono::DateTime<Local> {
    Local.timestamp_millis_opt(ts as i64).single().unwrap_or_else(Local::now)
}

fn open_external(path: &Path) -> bool {
    path.exists()
        && std::process::Command::new("xdg-open")
            .arg(path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .is_ok()
}

fn parse_key(key: &str) -> Option<Thread> {
    if let Some(uuid) = key.strip_prefix("c:") {
        let uuid = Uuid::parse_str(uuid).ok()?;
        Some(Thread::Contact(ServiceId::Aci(Aci::from(uuid))))
    } else {
        let bytes = hex::decode(key.strip_prefix("g:")?).ok()?;
        Some(Thread::Group(bytes.try_into().ok()?))
    }
}

impl App {
    pub fn new(mgr: Option<Mgr>, me: Uuid, tx: Tx, sidecar: Sidecar, att_dir: PathBuf, opts: Options,
               picker: Picker) -> Self {
        App {
            mgr,
            me,
            tx,
            sidecar,
            att_dir,
            opts,
            names: HashMap::new(),
            unknown: HashSet::new(),
            names_dirty: None,
            previews: HashMap::new(),
            chats: vec![],
            filter: String::new(),
            sel: 0,
            side_off: 0,
            current: None,
            msgs: vec![],
            pending: vec![],
            scroll: 0,
            input: Input::default(),
            files: vec![],
            last_dir: std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| "/".into()),
            focus: Focus::Side,
            modal: None,
            conn: Conn::Connecting,
            ready: false,
            asked_contacts: false,
            toast: None,
            picker,
            images: HashMap::new(),
            quit: false,
        }
    }

    // ── données ──────────────────────────────────────────────────────

    fn toast(&mut self, text: impl Into<String>, error: bool) {
        self.toast = Some((text.into(), error, Instant::now()));
    }

    fn name(&mut self, uuid: &Uuid) -> String {
        if *uuid == self.me {
            return "Moi".into();
        }
        if let Some(name) = self.names.get(uuid) {
            return name.clone();
        }
        if !uuid.is_nil() && self.unknown.insert(*uuid) {
            self.names_dirty = Some(Instant::now());
        }
        short(uuid)
    }

    /// Recharge contacts et groupes depuis le stockage local.
    async fn reload_directory(&mut self) {
        let Some(mgr) = self.mgr.clone() else { return };
        let store = mgr.store();
        let mut chats: HashMap<String, Chat> = HashMap::new();
        if let Ok(contacts) = store.contacts().await {
            for contact in contacts.flatten() {
                let phone = contact.phone_number.as_ref().map(|p| p.to_string()).unwrap_or_default();
                let name = if contact.uuid == self.me {
                    "Note à moi-même".to_string()
                } else if !contact.name.is_empty() {
                    contact.name.clone()
                } else {
                    phone.clone()
                };
                if name.is_empty() {
                    continue;
                }
                self.names.insert(contact.uuid, name.clone());
                let thread = Thread::Contact(ServiceId::Aci(Aci::from(contact.uuid)));
                let key = thread_key(&thread);
                let sub = if phone == name { String::new() } else { phone };
                chats.insert(key.clone(), Chat { key, thread, name, sub, last_ts: 0, unread: 0, preview: String::new() });
            }
        }
        if let Ok(groups) = store.groups().await {
            for (master_key, group) in groups.flatten() {
                let thread = Thread::Group(master_key);
                let key = thread_key(&thread);
                let name = if group.title.is_empty() { "Groupe sans nom".into() } else { group.title.clone() };
                let sub = format!("groupe · {} membres", group.members.len());
                chats.insert(key.clone(), Chat { key, thread, name, sub, last_ts: 0, unread: 0, preview: String::new() });
            }
        }
        let known: Vec<String> = self.sidecar.threads.keys().cloned().collect();
        for key in known {
            if chats.contains_key(&key) {
                continue;
            }
            let Some(thread) = parse_key(&key) else { continue };
            let (name, sub) = match &thread {
                Thread::Contact(id) => {
                    let uuid = id.raw_uuid();
                    if uuid == self.me {
                        ("Note à moi-même".to_string(), String::new())
                    } else {
                        (self.names.get(&uuid).cloned().unwrap_or_else(|| short(&uuid)), String::new())
                    }
                }
                Thread::Group(_) => ("Groupe".to_string(), "groupe".to_string()),
            };
            chats.insert(key.clone(), Chat { key, thread, name, sub, last_ts: 0, unread: 0, preview: String::new() });
        }
        for chat in chats.values_mut() {
            if let Some(meta) = self.sidecar.threads.get(&chat.key) {
                chat.last_ts = meta.last_ts;
                chat.unread = meta.unread;
            }
            if chat.last_ts > 0 && !self.previews.contains_key(&chat.key) {
                let mut preview = String::new();
                if let Ok(contents) = store.messages(&chat.thread, chat.last_ts..).await {
                    for content in contents.flatten() {
                        if let Incoming::Message { preview: p, .. } = model::classify(&content, self.me) {
                            preview = p;
                            break;
                        }
                    }
                }
                self.previews.insert(chat.key.clone(), preview);
            }
            chat.preview = self.previews.get(&chat.key).cloned().unwrap_or_default();
        }
        let selected = self.selected_key();
        self.chats = chats.into_values().collect();
        self.sort_chats(selected);
    }

    fn sort_chats(&mut self, keep: Option<String>) {
        self.chats.sort_by(|a, b| {
            b.last_ts.cmp(&a.last_ts).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        if let Some(key) = keep {
            if let Some(pos) = self.filtered().iter().position(|i| self.chats[*i].key == key) {
                self.sel = pos;
            }
        }
    }

    fn filtered(&self) -> Vec<usize> {
        let needle = self.filter.to_lowercase();
        self.chats
            .iter()
            .enumerate()
            .filter(|(_, c)| {
                needle.is_empty() || c.name.to_lowercase().contains(&needle) || c.sub.contains(&needle)
            })
            .map(|(i, _)| i)
            .collect()
    }

    fn selected_key(&self) -> Option<String> {
        self.filtered().get(self.sel).map(|i| self.chats[*i].key.clone())
    }

    fn chat_mut(&mut self, key: &str) -> Option<&mut Chat> {
        self.chats.iter_mut().find(|c| c.key == key)
    }

    fn current_chat(&self) -> Option<&Chat> {
        let key = self.current.as_deref()?;
        self.chats.iter().find(|c| c.key == key)
    }

    fn ensure_chat(&mut self, key: &str, thread: &Thread) {
        if self.chat_mut(key).is_some() {
            return;
        }
        let (name, sub) = match thread {
            Thread::Contact(id) => (self.name(&id.raw_uuid()), String::new()),
            Thread::Group(_) => ("Groupe".to_string(), "groupe".to_string()),
        };
        self.names_dirty = Some(Instant::now());
        self.chats.push(Chat {
            key: key.to_string(), thread: thread.clone(), name, sub, last_ts: 0, unread: 0, preview: String::new(),
        });
    }

    async fn reload_current(&mut self) {
        let (Some(mgr), Some(chat)) = (self.mgr.clone(), self.current_chat().cloned()) else { return };
        let Ok(contents) = mgr.store().messages(&chat.thread, ..).await else { return };
        // Le stockage rend les contenus du plus récent au plus ancien.
        let mut contents: Vec<_> = contents.flatten().take(MAX_CONTENTS).collect();
        contents.reverse();
        self.msgs = model::fold(&contents, self.me, &self.att_dir);
        let stored: HashSet<u64> = self.msgs.iter().map(|m| m.ts).collect();
        self.pending.retain(|(_, m)| !stored.contains(&m.ts));
    }

    async fn open_chat(&mut self, key: String) {
        self.current = Some(key);
        self.files.clear();
        self.scroll = 0;
        self.msgs.clear();
        self.reload_current().await;
        self.mark_read();
        self.focus = Focus::Chat;
    }

    fn mark_read(&mut self) {
        let Some(key) = self.current.clone() else { return };
        let last_read = self.sidecar.threads.get(&key).map(|m| m.last_read).unwrap_or(0);
        let mut by_sender: HashMap<Uuid, Vec<u64>> = HashMap::new();
        let mut newest = last_read;
        for msg in self.msgs.iter().filter(|m| !m.outgoing && !m.deleted && m.ts > last_read) {
            by_sender.entry(msg.sender).or_default().push(msg.ts);
            newest = newest.max(msg.ts);
        }
        let had_unread = self.sidecar.threads.get(&key).is_some_and(|m| m.unread > 0);
        if newest > last_read || had_unread {
            let meta = self.sidecar.meta(&key);
            meta.last_read = newest;
            meta.unread = 0;
        }
        if let Some(chat) = self.chat_mut(&key) {
            chat.unread = 0;
        }
        if let (true, Some(mgr), false) = (self.opts.read_receipts, self.mgr.clone(), by_sender.is_empty()) {
            tokio::task::spawn_local(signal::send_read_receipts(mgr, by_sender.into_iter().collect()));
        }
    }

    // ── événements Signal ────────────────────────────────────────────

    pub async fn on_event(&mut self, ev: Ev) {
        match ev {
            Ev::Conn(conn) => {
                if matches!(conn, Conn::Lost(_)) {
                    self.ready = false;
                }
                self.conn = conn;
            }
            Ev::Received(Received::QueueEmpty) => {
                self.conn = Conn::Ready;
                self.ready = true;
                self.reload_directory().await;
                self.reload_current().await;
                if self.chats.iter().all(|c| matches!(c.thread, Thread::Group(_))) && !self.asked_contacts {
                    self.asked_contacts = true;
                    self.request_contacts();
                }
            }
            Ev::Received(Received::Contacts) => {
                self.reload_directory().await;
                self.toast("Contacts synchronisés", false);
            }
            Ev::Received(Received::DecryptionError(sender)) => {
                let name = self.name(&sender.raw_uuid());
                self.toast(format!("Message illisible reçu de {name}"), true);
            }
            Ev::Received(Received::Content(content)) => self.on_content(&content).await,
            Ev::Downloaded { path, error, open } => {
                self.images.remove(&path);
                match error {
                    Some(error) => {
                        tracing::warn!(path = %path.display(), %error, "téléchargement échoué");
                        if open {
                            self.toast(format!("Téléchargement impossible : {error}"), true);
                        }
                    }
                    None if open => {
                        open_external(&path);
                    }
                    None => {}
                }
            }
            Ev::SendDone { key, ts, error } => {
                match error {
                    None => {
                        self.pending.retain(|(_, m)| m.ts != ts);
                        self.sidecar.meta(&key).last_ts = ts;
                        if let Some(chat) = self.chat_mut(&key) {
                            chat.last_ts = chat.last_ts.max(ts);
                        }
                        if self.current.as_deref() == Some(&key) {
                            self.reload_current().await;
                        }
                        let keep = self.selected_key();
                        self.sort_chats(keep);
                    }
                    Some(error) => {
                        if let Some((_, msg)) = self.pending.iter_mut().find(|(_, m)| m.ts == ts) {
                            msg.status = Status::Failed;
                        }
                        self.toast(format!("Échec de l'envoi : {error}"), true);
                    }
                }
            }
            Ev::Image { path, image } => {
                let state = image
                    .and_then(|img| {
                        self.picker
                            .new_protocol(img, Size::new(IMG_COLS, IMG_ROWS), Resize::Fit(None))
                            .ok()
                    })
                    .map(ImgState::Ready)
                    .unwrap_or(ImgState::Failed);
                self.images.insert(path, state);
            }
            Ev::Toast { text, error } => self.toast(text, error),
        }
    }

    async fn on_content(&mut self, content: &presage::libsignal_service::content::Content) {
        let Ok(thread) = Thread::try_from(content) else { return };
        let key = thread_key(&thread);
        let is_current = self.current.as_deref() == Some(key.as_str());
        match model::classify(content, self.me) {
            Incoming::Receipt { read, timestamps } => {
                for ts in timestamps {
                    self.sidecar.set_status(ts, if read { 2 } else { 1 });
                }
            }
            Incoming::Update if is_current => self.reload_current().await,
            Incoming::Message { outgoing, preview } => {
                self.ensure_chat(&key, &thread);
                let ts = content.timestamp();
                self.previews.insert(key.clone(), preview.clone());
                let meta = self.sidecar.meta(&key);
                meta.last_ts = meta.last_ts.max(ts);
                if outgoing {
                    // Répondu depuis un autre appareil : la conversation y a été lue.
                    meta.unread = 0;
                    meta.last_read = meta.last_read.max(ts);
                } else if !is_current {
                    meta.unread += 1;
                }
                let (last_ts, unread) = (meta.last_ts, meta.unread);
                let mut title = String::new();
                if let Some(chat) = self.chat_mut(&key) {
                    chat.last_ts = last_ts;
                    chat.unread = unread;
                    chat.preview = preview.clone();
                    title = chat.name.clone();
                }
                if is_current {
                    self.reload_current().await;
                    if !outgoing {
                        self.mark_read();
                    }
                } else if !outgoing && self.ready {
                    if matches!(thread, Thread::Group(_)) {
                        let sender = self.name(&content.metadata.sender.raw_uuid());
                        title = format!("{sender} @ {title}");
                    }
                    let _ = std::io::stdout().write_all(b"\x07");
                    self.toast(format!("{title} : {preview}"), false);
                    if self.opts.notify {
                        let _ = std::process::Command::new("notify-send")
                            .args(["-a", "tuisignal", &title, &preview])
                            .stdout(std::process::Stdio::null())
                            .stderr(std::process::Stdio::null())
                            .spawn();
                    }
                }
                let keep = self.selected_key();
                self.sort_chats(keep);
            }
            _ => {}
        }
    }

    async fn on_tick(&mut self) {
        self.sidecar.save_if_dirty();
        if self.names_dirty.is_some_and(|t| t.elapsed() > Duration::from_secs(3)) {
            self.names_dirty = None;
            self.reload_directory().await;
        }
    }

    fn request_contacts(&mut self) {
        if let Some(mgr) = self.mgr.clone() {
            tokio::task::spawn_local(signal::request_contacts(mgr, self.tx.clone()));
        }
    }

    // ── clavier ──────────────────────────────────────────────────────

    pub async fn on_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && matches!(key.code, KeyCode::Char('q') | KeyCode::Char('c')) {
            self.quit = true;
            return;
        }
        if let Some(modal) = self.modal.take() {
            self.modal_key(modal, key);
            return;
        }
        match key.code {
            KeyCode::Char('o') if ctrl => {
                if self.current.is_some() {
                    self.modal = Some(Modal::Files(FilePicker::new(self.last_dir.clone())));
                } else {
                    self.toast("Choisissez d'abord une conversation.", true);
                }
            }
            KeyCode::Char('p') if ctrl => self.open_att_list(),
            KeyCode::Char('r') if ctrl => {
                self.focus = Focus::Side;
                self.filter.clear();
                self.sel = 0;
            }
            KeyCode::F(5) => {
                self.request_contacts();
                self.toast("Synchronisation des contacts demandée…", false);
            }
            KeyCode::Tab => {
                self.focus = match self.focus {
                    Focus::Side if self.current.is_some() => Focus::Chat,
                    _ => Focus::Side,
                }
            }
            _ => match self.focus {
                Focus::Side => self.side_key(key).await,
                Focus::Chat => self.chat_key(key),
            },
        }
    }

    async fn side_key(&mut self, key: KeyEvent) {
        let count = self.filtered().len();
        let last = count.saturating_sub(1);
        match key.code {
            KeyCode::Up => self.sel = self.sel.saturating_sub(1),
            KeyCode::Down => self.sel = (self.sel + 1).min(last),
            KeyCode::PageUp => self.sel = self.sel.saturating_sub(10),
            KeyCode::PageDown => self.sel = (self.sel + 10).min(last),
            KeyCode::Home => self.sel = 0,
            KeyCode::End => self.sel = last,
            KeyCode::Enter => {
                if let Some(key) = self.selected_key() {
                    self.filter.clear();
                    self.open_chat(key.clone()).await;
                    self.sort_chats(Some(key));
                }
            }
            KeyCode::Esc => {
                if !self.filter.is_empty() {
                    self.filter.clear();
                    self.sel = 0;
                } else if self.current.is_some() {
                    self.focus = Focus::Chat;
                }
            }
            KeyCode::Backspace => {
                self.filter.pop();
                self.sel = 0;
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.filter.push(c);
                self.sel = 0;
            }
            _ => {}
        }
    }

    fn chat_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Enter if !key.modifiers.contains(KeyModifiers::ALT) => self.submit(),
            KeyCode::Esc => {
                if self.files.is_empty() {
                    self.focus = Focus::Side;
                } else {
                    self.files.clear();
                }
            }
            KeyCode::PageUp => self.scroll += 10,
            KeyCode::PageDown => self.scroll = self.scroll.saturating_sub(10),
            KeyCode::Up => self.scroll += 1,
            KeyCode::Down => self.scroll = self.scroll.saturating_sub(1),
            _ => self.input.key(key),
        }
    }

    pub fn on_paste(&mut self, text: String) {
        if self.modal.is_some() {
            return;
        }
        let paths: Vec<PathBuf> =
            text.lines().filter(|l| !l.trim().is_empty()).map(clean_path).collect();
        if !paths.is_empty() && paths.iter().all(|p| p.is_absolute() && p.is_file()) {
            // Glisser-déposer d'un fichier dans le terminal = collage de son chemin.
            self.add_files(paths);
        } else if self.focus == Focus::Chat {
            self.input.insert_str(&text);
        } else {
            self.filter.push_str(text.lines().next().unwrap_or(""));
            self.sel = 0;
        }
    }

    fn add_files(&mut self, paths: Vec<PathBuf>) {
        if self.current.is_none() {
            self.toast("Choisissez d'abord une conversation.", true);
            return;
        }
        for path in paths {
            if !self.files.contains(&path) {
                self.files.push(path);
            }
        }
        self.focus = Focus::Chat;
    }

    fn submit(&mut self) {
        let text = self.input.text().trim().to_string();
        if let Some(rest) = text.strip_prefix("/attach ") {
            let path = clean_path(rest);
            if path.is_file() {
                self.input.take();
                self.add_files(vec![path]);
            } else {
                self.toast(format!("Fichier introuvable : {}", path.display()), true);
            }
            return;
        }
        if text.is_empty() && self.files.is_empty() {
            return;
        }
        let Some(chat) = self.current_chat().cloned() else {
            self.toast("Choisissez d'abord une conversation.", true);
            return;
        };
        if !self.ready {
            self.toast("Pas encore synchronisé avec Signal, réessayez dans un instant.", true);
            return;
        }
        self.input.take();
        let ts = now_ms();
        let mut atts = Vec::new();
        let mut to_send = Vec::new();
        for (idx, source) in std::mem::take(&mut self.files).into_iter().enumerate() {
            let name = source.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let mime = mime_guess::from_path(&name).first_or_octet_stream().to_string();
            let pointer = AttachmentPointer {
                file_name: Some(name.clone()),
                content_type: Some(mime.clone()),
                ..Default::default()
            };
            // Copie locale au même emplacement que les pièces reçues, pour l'historique.
            let dest = att_path(&self.att_dir, ts, idx, &pointer);
            let path = if std::fs::copy(&source, &dest).is_ok() { dest } else { source };
            let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            atts.push(Att { name: att_name(&pointer), mime, size, path: path.clone(), pointer: None });
            to_send.push((path, name));
        }
        let msg = Msg {
            ts,
            alt_ts: vec![],
            sender: self.me,
            outgoing: true,
            body: text.clone(),
            atts,
            reactions: vec![],
            edited: false,
            deleted: false,
            status: Status::Sending,
        };
        self.previews.insert(chat.key.clone(), msg.preview());
        if let Some(c) = self.chat_mut(&chat.key) {
            c.preview = msg.preview();
        }
        self.pending.push((chat.key.clone(), msg));
        self.scroll = 0;
        if let Some(mgr) = self.mgr.clone() {
            let tx = self.tx.clone();
            tokio::task::spawn_local(async move {
                let error = signal::send(mgr, chat.thread, text, to_send, ts).await.err();
                let _ = tx.send(Ev::SendDone { key: chat.key, ts, error: error.map(|e| e.to_string()) });
            });
        }
    }

    fn open_att_list(&mut self) {
        if self.current.is_none() {
            return;
        }
        let mut items = Vec::new();
        let msgs: Vec<Msg> = self.visible_msgs().cloned().collect();
        for msg in msgs.iter().rev() {
            let who = self.name(&msg.sender);
            for att in &msg.atts {
                let when = local(msg.ts).format("%d/%m %H:%M");
                items.push((att.clone(), format!("{} · {who} · {when}", human_size(att.size))));
            }
        }
        self.modal = Some(Modal::Atts(AttList { items, sel: 0 }));
    }

    fn modal_key(&mut self, modal: Modal, key: KeyEvent) {
        if key.code == KeyCode::Esc {
            return;
        }
        match modal {
            Modal::Files(mut picker) => {
                let count = picker.visible().len();
                match key.code {
                    KeyCode::Up => picker.sel = picker.sel.saturating_sub(1),
                    KeyCode::Down => picker.sel = (picker.sel + 1).min(count.saturating_sub(1)),
                    KeyCode::PageUp => picker.sel = picker.sel.saturating_sub(10),
                    KeyCode::PageDown => picker.sel = (picker.sel + 10).min(count.saturating_sub(1)),
                    KeyCode::Left => picker.parent(),
                    KeyCode::Backspace => {
                        if picker.filter.pop().is_none() {
                            picker.parent();
                        }
                        picker.sel = 0;
                    }
                    KeyCode::Enter | KeyCode::Right => {
                        let chosen = picker.visible().get(picker.sel).map(|(n, d)| (n.clone(), *d));
                        if let Some((name, is_dir)) = chosen {
                            let path = picker.dir.join(name);
                            if is_dir {
                                picker.enter(path);
                            } else if key.code == KeyCode::Enter {
                                self.last_dir = picker.dir.clone();
                                self.add_files(vec![path]);
                                return;
                            }
                        }
                    }
                    KeyCode::Char('~') if picker.filter.is_empty() => {
                        if let Some(home) = std::env::var_os("HOME") {
                            picker.enter(home.into());
                        }
                    }
                    KeyCode::Char('/') if picker.filter.is_empty() => picker.enter("/".into()),
                    KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                        picker.filter.push(c);
                        picker.sel = 0;
                    }
                    _ => {}
                }
                self.modal = Some(Modal::Files(picker));
            }
            Modal::Atts(mut list) => {
                match key.code {
                    KeyCode::Up => list.sel = list.sel.saturating_sub(1),
                    KeyCode::Down => list.sel = (list.sel + 1).min(list.items.len().saturating_sub(1)),
                    KeyCode::Enter => {
                        if let Some((att, _)) = list.items.get(list.sel) {
                            if att.path.exists() {
                                if open_external(&att.path) {
                                    self.toast(format!("Ouverture de {}", att.name), false);
                                } else {
                                    self.toast("xdg-open est introuvable.", true);
                                }
                            } else if let (Some(pointer), Some(mgr)) = (att.pointer.clone(), self.mgr.clone()) {
                                self.toast(format!("Téléchargement de {}…", att.name), false);
                                tokio::task::spawn_local(signal::download(
                                    mgr, self.tx.clone(), pointer, att.path.clone(), true,
                                ));
                            } else {
                                self.toast("Fichier introuvable sur le disque.", true);
                            }
                        }
                    }
                    _ => {}
                }
                self.modal = Some(Modal::Atts(list));
            }
        }
    }

    // ── affichage ────────────────────────────────────────────────────

    fn visible_msgs(&self) -> impl Iterator<Item = &Msg> {
        let key = self.current.clone().unwrap_or_default();
        self.msgs.iter().chain(self.pending.iter().filter(move |(k, _)| *k == key).map(|(_, m)| m))
    }

    fn status_of(&self, msg: &Msg) -> Status {
        if msg.status != Status::Sent {
            return msg.status;
        }
        let level = std::iter::once(&msg.ts)
            .chain(&msg.alt_ts)
            .filter_map(|ts| self.sidecar.status.get(ts))
            .max()
            .copied()
            .unwrap_or(0);
        match level {
            2 => Status::Read,
            1 => Status::Delivered,
            _ => Status::Sent,
        }
    }

    fn build_rows(&mut self, width: usize) -> Vec<Row> {
        let dim = Style::new().fg(Color::DarkGray);
        let msgs: Vec<Msg> = self.visible_msgs().cloned().collect();
        let mut rows = Vec::new();
        let mut day = None;
        for msg in &msgs {
            let when = local(msg.ts);
            if day != Some(when.date_naive()) {
                day = Some(when.date_naive());
                let label = format!(
                    "── {} {} {} {} ──",
                    DAYS[when.weekday().num_days_from_monday() as usize],
                    when.day(),
                    MONTHS[when.month0() as usize],
                    when.year()
                );
                rows.push(Row::Text(Line::styled(label, dim.add_modifier(Modifier::ITALIC))));
            }
            let mut header = if msg.outgoing {
                vec![Span::styled("Moi", Style::new().fg(Color::Green).add_modifier(Modifier::BOLD))]
            } else if msg.sender.is_nil() {
                vec![]
            } else {
                let color = NAME_COLORS[msg.sender.as_bytes().iter().map(|b| *b as usize).sum::<usize>() % NAME_COLORS.len()];
                vec![Span::styled(self.name(&msg.sender), Style::new().fg(color).add_modifier(Modifier::BOLD))]
            };
            header.push(Span::styled(format!("  {}", when.format("%H:%M")), dim));
            if msg.edited {
                header.push(Span::styled("  modifié", dim));
            }
            if msg.outgoing {
                let (glyph, style) = match self.status_of(msg) {
                    Status::Sending => ("  …", dim),
                    Status::Sent => ("  ✓", dim),
                    Status::Delivered => ("  ✓✓", dim),
                    Status::Read => ("  ✓✓ lu", Style::new().fg(Color::Cyan)),
                    Status::Failed => ("  ✗ échec de l'envoi", Style::new().fg(Color::Red).add_modifier(Modifier::BOLD)),
                };
                header.push(Span::styled(glyph, style));
            }
            rows.push(Row::Text(Line::from(header)));
            for paragraph in msg.body.split('\n').filter(|_| !msg.body.is_empty()) {
                let style = if msg.deleted {
                    dim.add_modifier(Modifier::ITALIC)
                } else if paragraph.starts_with("> ") {
                    dim
                } else {
                    Style::new()
                };
                if paragraph.is_empty() {
                    rows.push(Row::Text(Line::default()));
                }
                for line in textwrap::wrap(paragraph, width.max(8)) {
                    rows.push(Row::Text(Line::styled(line.into_owned(), style)));
                }
            }
            if !msg.reactions.is_empty() {
                let text = msg
                    .reactions
                    .clone()
                    .iter()
                    .map(|(who, emoji)| format!("{emoji} {}", self.name(who)))
                    .collect::<Vec<_>>()
                    .join("   ");
                rows.push(Row::Text(Line::styled(fit(&text, width), dim)));
            }
            for att in &msg.atts {
                let present = att.path.exists();
                if att.is_image() && present {
                    for part in 0..IMG_ROWS {
                        rows.push(Row::Image { path: att.path.clone(), part });
                    }
                }
                let mut spans = vec![
                    Span::raw("📎 "),
                    Span::styled(fit(&att.name, width.saturating_sub(24).max(8)), Style::new().fg(Color::Yellow).add_modifier(Modifier::UNDERLINED)),
                    Span::styled(format!("  {}", human_size(att.size)), dim),
                ];
                if !present {
                    spans.push(Span::styled("  non téléchargé", dim));
                }
                rows.push(Row::Text(Line::from(spans)));
            }
            rows.push(Row::Text(Line::default()));
        }
        rows
    }

    pub fn draw(&mut self, f: &mut Frame) {
        let [body, footer] = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(f.area());
        let [side, main] =
            Layout::horizontal([Constraint::Length(SIDE_WIDTH), Constraint::Min(0)]).areas(body);
        self.draw_side(f, side);
        self.draw_main(f, main);
        self.draw_footer(f, footer);
        match self.modal.take() {
            Some(Modal::Files(picker)) => {
                self.draw_files(f, &picker);
                self.modal = Some(Modal::Files(picker));
            }
            Some(Modal::Atts(list)) => {
                self.draw_atts(f, &list);
                self.modal = Some(Modal::Atts(list));
            }
            None => {}
        }
    }

    fn draw_side(&mut self, f: &mut Frame, area: Rect) {
        let focused = self.focus == Focus::Side && self.modal.is_none();
        let border = if focused { Color::Yellow } else { Color::DarkGray };
        let block = Block::new().borders(Borders::RIGHT).border_style(Style::new().fg(border));
        let inner = block.inner(area);
        f.render_widget(block, area);
        if inner.height < 2 || inner.width < 4 {
            return;
        }
        let width = inner.width as usize;
        let search = if focused || !self.filter.is_empty() {
            Line::from(vec![Span::styled("/ ", Style::new().fg(Color::Yellow)), Span::raw(fit(&self.filter, width - 3))])
        } else {
            Line::styled("Ctrl+R : rechercher", Style::new().fg(Color::DarkGray))
        };
        f.render_widget(Paragraph::new(search), Rect { height: 1, ..inner });
        if focused {
            let x = inner.x + 2 + self.filter.width().min(width - 3) as u16;
            f.set_cursor_position(Position::new(x, inner.y));
        }

        let list = Rect { y: inner.y + 1, height: inner.height - 1, ..inner };
        let indices = self.filtered();
        self.sel = self.sel.min(indices.len().saturating_sub(1));
        let per_page = (list.height as usize / 2).max(1);
        if self.sel < self.side_off {
            self.side_off = self.sel;
        } else if self.sel >= self.side_off + per_page {
            self.side_off = self.sel + 1 - per_page;
        }
        let mut lines = Vec::new();
        for (pos, idx) in indices.iter().enumerate().skip(self.side_off).take(per_page) {
            let chat = &self.chats[*idx];
            let is_current = self.current.as_deref() == Some(chat.key.as_str());
            let mut style = Style::new();
            if chat.unread > 0 || is_current {
                style = style.add_modifier(Modifier::BOLD);
            }
            if is_current {
                style = style.fg(Color::Green);
            }
            let selected = pos == self.sel;
            let line_style = if selected && focused {
                Style::new().bg(Color::DarkGray)
            } else {
                Style::new()
            };
            let badge = if chat.unread > 0 { format!(" ({})", chat.unread) } else { String::new() };
            let prefix = match (&chat.thread, selected) {
                (_, true) => "› ",
                (Thread::Group(_), false) => "# ",
                _ => "  ",
            };
            let name = fit(&chat.name, width.saturating_sub(2 + badge.width()));
            lines.push(
                Line::from(vec![
                    Span::styled(prefix, Style::new().fg(Color::Yellow)),
                    Span::styled(name, style),
                    Span::styled(badge, Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
                ])
                .style(line_style),
            );
            let second = if chat.preview.is_empty() { &chat.sub } else { &chat.preview };
            lines.push(
                Line::styled(format!("  {}", fit(second, width - 2)), Style::new().fg(Color::Gray))
                    .style(line_style.fg(Color::Gray)),
            );
        }
        if indices.is_empty() {
            let hint = if self.chats.is_empty() { "Aucun contact pour l'instant" } else { "Aucun résultat" };
            lines.push(Line::styled(hint, Style::new().fg(Color::DarkGray)));
        }
        f.render_widget(Paragraph::new(lines), list);
    }

    fn draw_main(&mut self, f: &mut Frame, area: Rect) {
        if area.width < 10 || area.height < 6 {
            return;
        }
        let files_height = if self.files.is_empty() { 0 } else { 1 };
        let [title, messages, files, input] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(files_height),
            Constraint::Length(3),
        ])
        .areas(area);

        // Titre et état de la connexion.
        let (state, color) = match &self.conn {
            Conn::Connecting => ("connexion…".to_string(), Color::Yellow),
            Conn::Syncing => ("synchronisation…".to_string(), Color::Yellow),
            Conn::Ready => ("● connecté".to_string(), Color::Green),
            Conn::Lost(why) => (format!("hors ligne : {}", fit(why, 40)), Color::Red),
        };
        let mut spans = match self.current_chat() {
            Some(chat) => vec![
                Span::styled(format!(" {}", chat.name), Style::new().add_modifier(Modifier::BOLD)),
                Span::styled(format!("  {}", chat.sub), Style::new().fg(Color::Gray)),
            ],
            None => vec![Span::styled(" tuisignal", Style::new().add_modifier(Modifier::BOLD))],
        };
        let used: usize = spans.iter().map(|s| s.content.width()).sum();
        let pad = (title.width as usize).saturating_sub(used + state.width() + 1);
        spans.push(Span::raw(" ".repeat(pad)));
        spans.push(Span::styled(state, Style::new().fg(color)));
        f.render_widget(Paragraph::new(Line::from(spans)).style(Style::new().bg(Color::Black)), title);

        // Messages.
        let view = Rect { x: messages.x + 1, width: messages.width - 2, ..messages };
        if self.current.is_none() {
            let help = vec![
                Line::default(),
                Line::raw("Choisissez une conversation à gauche (↑ ↓ puis Entrée),"),
                Line::raw("ou tapez directement pour filtrer la liste."),
            ];
            f.render_widget(Paragraph::new(help).style(Style::new().fg(Color::Gray)), view);
        } else {
            self.draw_messages(f, view);
        }

        if files_height > 0 {
            let names: Vec<String> = self
                .files
                .iter()
                .map(|p| {
                    let size = std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
                    format!("{} ({})", p.file_name().unwrap_or_default().to_string_lossy(), human_size(size))
                })
                .collect();
            let text = format!(" 📎 {} — Entrée : envoyer · Échap : retirer", names.join(", "));
            f.render_widget(
                Paragraph::new(fit(&text, files.width as usize)).style(Style::new().fg(Color::Yellow)),
                files,
            );
        }

        // Saisie.
        let focused = self.focus == Focus::Chat && self.modal.is_none();
        let block = Block::bordered()
            .border_style(Style::new().fg(if focused { Color::Yellow } else { Color::DarkGray }));
        let inner = block.inner(input);
        f.render_widget(block, input);
        let (text, cursor) = self.input.view(inner.width.saturating_sub(1) as usize);
        if text.is_empty() && !focused {
            f.render_widget(
                Paragraph::new("Message — Entrée pour envoyer").style(Style::new().fg(Color::DarkGray)),
                inner,
            );
        } else {
            f.render_widget(Paragraph::new(text), inner);
        }
        if focused {
            f.set_cursor_position(Position::new(inner.x + cursor, inner.y));
        }
    }

    fn draw_messages(&mut self, f: &mut Frame, area: Rect) {
        let rows = self.build_rows(area.width as usize);
        let height = area.height as usize;
        self.scroll = self.scroll.min(rows.len().saturating_sub(height));
        let end = rows.len() - self.scroll;
        let start = end.saturating_sub(height);
        let visible = &rows[start..end];
        // Les conversations courtes restent collées en bas, près de la saisie.
        let top = area.y + (height - visible.len()) as u16;

        let mut lines = Vec::with_capacity(visible.len());
        let mut images = Vec::new();
        for (i, row) in visible.iter().enumerate() {
            match row {
                Row::Text(line) => lines.push(line.clone()),
                Row::Image { path, part } => {
                    let whole = *part == 0 && i + IMG_ROWS as usize <= visible.len();
                    if *part == 0 && !self.images.contains_key(path) {
                        self.images.insert(path.clone(), ImgState::Loading);
                        signal::decode_image(path.clone(), self.tx.clone());
                    }
                    let label = match (part, self.images.get(path)) {
                        (0, Some(ImgState::Loading)) => "  chargement de l'image…",
                        (0, Some(ImgState::Failed)) => "  (aperçu indisponible)",
                        (0, Some(ImgState::Ready(_))) if whole => {
                            images.push((path.clone(), top + i as u16));
                            ""
                        }
                        _ => "",
                    };
                    lines.push(Line::styled(label, Style::new().fg(Color::DarkGray)));
                }
            }
        }
        let text_area = Rect { y: top, height: visible.len() as u16, ..area };
        f.render_widget(Paragraph::new(lines), text_area);
        for (path, y) in images {
            if let Some(ImgState::Ready(protocol)) = self.images.get(&path) {
                let rect = Rect { x: area.x, y, width: IMG_COLS.min(area.width), height: IMG_ROWS };
                f.render_widget(Image::new(protocol), rect);
            }
        }
        if self.scroll > 0 {
            let label = format!(" ↓ {} lignes ", self.scroll);
            let w = label.width() as u16;
            if area.width > w {
                let rect = Rect { x: area.x + area.width - w, y: area.y + area.height - 1, width: w, height: 1 };
                f.render_widget(Paragraph::new(label).style(Style::new().bg(Color::DarkGray)), rect);
            }
        }
    }

    fn draw_footer(&mut self, f: &mut Frame, area: Rect) {
        if let Some((_, error, since)) = &self.toast {
            if since.elapsed() > Duration::from_secs(if *error { 10 } else { 5 }) {
                self.toast = None;
            }
        }
        let line = match &self.toast {
            Some((text, error, _)) => {
                let color = if *error { Color::Red } else { Color::Cyan };
                Line::styled(format!(" {}", fit(&text.replace('\n', " "), area.width.saturating_sub(2) as usize)),
                    Style::new().fg(color).add_modifier(Modifier::BOLD))
            }
            None => {
                let key = |k: &'static str| Span::styled(k, Style::new().fg(Color::Yellow));
                let mut spans = Vec::new();
                for (k, label) in [
                    ("^Q", "quitter"), ("^R", "rechercher"), ("^O", "joindre"), ("^P", "pièces jointes"),
                    ("Tab", "zone"), ("PgUp/PgDn", "défiler"), ("F5", "synchroniser"),
                ] {
                    spans.push(Span::raw(" "));
                    spans.push(key(k));
                    spans.push(Span::styled(format!(" {label} "), Style::new().fg(Color::Gray)));
                }
                Line::from(spans)
            }
        };
        f.render_widget(Paragraph::new(line), area);
    }

    fn modal_area(&self, f: &mut Frame, title: String) -> Rect {
        let area = f.area();
        let w = (area.width * 4 / 5).max(area.width.min(40));
        let h = (area.height * 4 / 5).max(area.height.min(10));
        let rect = Rect { x: (area.width - w) / 2, y: (area.height - h) / 2, width: w, height: h };
        f.render_widget(Clear, rect);
        let block = Block::bordered()
            .title(title)
            .border_style(Style::new().fg(Color::Yellow));
        let inner = block.inner(rect);
        f.render_widget(block, rect);
        inner
    }

    fn draw_list(f: &mut Frame, area: Rect, lines: Vec<Line<'static>>, sel: usize) {
        let height = area.height as usize;
        let off = if sel >= height { sel + 1 - height } else { 0 };
        let shown: Vec<Line> = lines
            .into_iter()
            .enumerate()
            .skip(off)
            .take(height)
            .map(|(i, line)| if i == sel { line.style(Style::new().bg(Color::DarkGray)) } else { line })
            .collect();
        f.render_widget(Paragraph::new(shown), area);
    }

    fn draw_files(&self, f: &mut Frame, picker: &FilePicker) {
        let inner = self.modal_area(f, format!(" Joindre un fichier — {} ", picker.dir.display()));
        if inner.height < 3 {
            return;
        }
        let help = Line::styled(
            "↑↓ choisir · Entrée ouvrir/joindre · ← dossier parent · ~ dossier personnel · Échap annuler",
            Style::new().fg(Color::Gray),
        );
        f.render_widget(Paragraph::new(help), Rect { height: 1, ..inner });
        let filter = Line::from(vec![
            Span::styled("Filtre : ", Style::new().fg(Color::Yellow)),
            Span::raw(picker.filter.clone()),
        ]);
        f.render_widget(Paragraph::new(filter), Rect { y: inner.y + 1, height: 1, ..inner });
        f.set_cursor_position(Position::new(inner.x + 9 + picker.filter.width() as u16, inner.y + 1));
        let lines: Vec<Line> = picker
            .visible()
            .iter()
            .map(|(name, is_dir)| {
                if *is_dir {
                    Line::styled(format!("▸ {name}/"), Style::new().fg(Color::Cyan))
                } else {
                    Line::raw(format!("  {name}"))
                }
            })
            .collect();
        let list = Rect { y: inner.y + 2, height: inner.height - 2, ..inner };
        if lines.is_empty() {
            f.render_widget(Paragraph::new("(vide)").style(Style::new().fg(Color::DarkGray)), list);
        } else {
            Self::draw_list(f, list, lines, picker.sel);
        }
    }

    fn draw_atts(&self, f: &mut Frame, list: &AttList) {
        let inner = self.modal_area(f, " Pièces jointes de la conversation ".to_string());
        if inner.height < 2 {
            return;
        }
        let help = Line::styled("↑↓ choisir · Entrée ouvrir (télécharge si besoin) · Échap fermer",
            Style::new().fg(Color::Gray));
        f.render_widget(Paragraph::new(help), Rect { height: 1, ..inner });
        let area = Rect { y: inner.y + 1, height: inner.height - 1, ..inner };
        if list.items.is_empty() {
            f.render_widget(
                Paragraph::new("Aucune pièce jointe dans cette conversation.").style(Style::new().fg(Color::DarkGray)),
                area,
            );
            return;
        }
        let lines = list
            .items
            .iter()
            .map(|(att, info)| {
                let missing = if att.path.exists() { "" } else { " · non téléchargé" };
                Line::from(vec![
                    Span::styled(format!(" {}", att.name), Style::new().add_modifier(Modifier::BOLD)),
                    Span::styled(format!("  {info}{missing}"), Style::new().fg(Color::Gray)),
                ])
            })
            .collect();
        Self::draw_list(f, area, lines, list.sel);
    }
}

pub async fn run(mgr: Mgr, att_dir: PathBuf, sidecar: Sidecar, opts: Options) -> anyhow::Result<()> {
    let me = mgr.registration_data().service_ids.aci;
    let (tx, mut rx) = unbounded_channel();
    let mut terminal = ratatui::init();
    let _ = crossterm::execute!(std::io::stdout(), EnableBracketedPaste);
    // Détecte le protocole graphique du terminal (Kitty, Sixel, iTerm2), sinon demi-blocs.
    let picker = Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks());

    let mut app = App::new(Some(mgr.clone()), me, tx.clone(), sidecar, att_dir.clone(), opts, picker);
    app.reload_directory().await;
    tokio::task::spawn_local(signal::receive_loop(mgr, tx, att_dir));

    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    let result = loop {
        if let Err(error) = terminal.draw(|f| app.draw(f)) {
            break Err(error.into());
        }
        tokio::select! {
            Some(event) = events.next() => match event {
                Ok(Event::Key(key)) if key.kind != KeyEventKind::Release => app.on_key(key).await,
                Ok(Event::Paste(text)) => app.on_paste(text),
                Ok(_) => {}
                Err(error) => break Err(error.into()),
            },
            Some(ev) = rx.recv() => {
                app.on_event(ev).await;
                // Pendant la synchronisation initiale, traite par lots avant de redessiner.
                for _ in 0..200 {
                    match rx.try_recv() {
                        Ok(ev) => app.on_event(ev).await,
                        Err(_) => break,
                    }
                }
            }
            _ = tick.tick() => app.on_tick().await,
        }
        if app.quit {
            break Ok(());
        }
    };
    app.sidecar.save_if_dirty();
    let _ = crossterm::execute!(std::io::stdout(), DisableBracketedPaste);
    ratatui::restore();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn screen(terminal: &Terminal<TestBackend>) -> String {
        let buffer = terminal.backend().buffer();
        let mut out = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                out.push_str(buffer[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn msg(ts: u64, sender: Uuid, outgoing: bool, body: &str) -> Msg {
        Msg {
            ts, alt_ts: vec![], sender, outgoing, body: body.into(), atts: vec![], reactions: vec![],
            edited: false, deleted: false, status: Status::Sent,
        }
    }

    fn app() -> (App, tokio::sync::mpsc::UnboundedReceiver<Ev>) {
        let (tx, rx) = unbounded_channel();
        let me = Uuid::from_bytes([9; 16]);
        let dir = std::env::temp_dir().join(format!("tuisignal-test-{}", now_ms()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut app = App::new(None, me, tx, Sidecar::default(), dir,
            Options { read_receipts: false, notify: false }, Picker::halfblocks());
        for (n, name, last_ts, unread) in [(1u8, "Alice Martin", 50, 0), (2, "Bob", 10, 2)] {
            let thread = Thread::Contact(ServiceId::Aci(Aci::from(Uuid::from_bytes([n; 16]))));
            app.chats.push(Chat {
                key: thread_key(&thread), thread, name: name.into(), sub: "+33600000000".into(),
                last_ts, unread, preview: "dernier message".into(),
            });
            app.names.insert(Uuid::from_bytes([n; 16]), name.into());
        }
        let group = Thread::Group([7; 32]);
        app.chats.push(Chat {
            key: thread_key(&group), thread: group, name: "Famille".into(), sub: "groupe · 4 membres".into(),
            last_ts: 0, unread: 0, preview: String::new(),
        });
        app.sort_chats(None);
        (app, rx)
    }

    #[tokio::test]
    async fn renders_and_navigates() {
        let (mut app, mut rx) = app();
        let mut terminal = Terminal::new(TestBackend::new(110, 30)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        let text = screen(&terminal);
        assert!(text.contains("Alice Martin") && text.contains("Bob (2)") && text.contains("# Famille"), "{text}");
        assert!(text.contains("Choisissez une conversation"));

        // Filtre « bob » puis Entrée : ouvre la conversation et passe à la saisie.
        for c in "bob".chars() {
            app.on_key(key(KeyCode::Char(c))).await;
        }
        app.on_key(key(KeyCode::Enter)).await;
        let bob = app.current.clone().unwrap();
        assert!(bob.starts_with("c:02020202"));
        assert!(app.focus == Focus::Chat);

        let alice = Uuid::from_bytes([2; 16]);
        app.msgs = vec![msg(1_700_000_000_000, alice, false, "Salut !\nÇa va ?"), msg(1_700_000_060_000, app.me, true, "Oui et toi ?")];
        app.msgs[1].reactions.push((alice, "👍".into()));
        app.sidecar.set_status(1_700_000_060_000, 2);
        terminal.draw(|f| app.draw(f)).unwrap();
        let text = screen(&terminal);
        for needle in ["Salut !", "Ça va ?", "Oui et toi ?", "✓✓ lu", "novembre 2023", "Bob", "Moi"] {
            assert!(text.contains(needle), "manque {needle:?} dans\n{text}");
        }

        // Sans synchronisation terminée, l'envoi est refusé et le texte conservé.
        for c in "coucou".chars() {
            app.on_key(key(KeyCode::Char(c))).await;
        }
        app.on_key(key(KeyCode::Enter)).await;
        assert_eq!(app.input.text(), "coucou");
        assert!(app.pending.is_empty());

        // Une fois prêt : message en attente avec une pièce jointe, copiée dans le dossier local.
        app.ready = true;
        let sources = app.att_dir.join("sources");
        std::fs::create_dir_all(&sources).unwrap();
        let file = sources.join("source photo.txt");
        std::fs::write(&file, "bonjour").unwrap();
        app.on_paste(format!("'{}'", file.display()));
        assert_eq!(app.files, vec![file.clone()]);
        terminal.draw(|f| app.draw(f)).unwrap();
        assert!(screen(&terminal).contains("source photo.txt (7 o)"));
        app.on_key(key(KeyCode::Enter)).await;
        assert!(app.input.text().is_empty() && app.files.is_empty());
        let (pending_key, pending) = &app.pending[0];
        assert_eq!(pending_key, &bob);
        assert_eq!((pending.body.as_str(), pending.status), ("coucou", Status::Sending));
        assert!(pending.atts[0].path.exists() && pending.atts[0].path != file);
        let ts = pending.ts;
        terminal.draw(|f| app.draw(f)).unwrap();
        let text = screen(&terminal);
        assert!(text.contains("coucou") && text.contains("source photo.txt"), "{text}");

        // Échec d'envoi signalé.
        app.on_event(Ev::SendDone { key: bob.clone(), ts, error: Some("réseau".into()) }).await;
        terminal.draw(|f| app.draw(f)).unwrap();
        let text = screen(&terminal);
        assert!(text.contains("échec de l'envoi") && text.contains("Échec de l'envoi : réseau"), "{text}");

        // Fenêtres modales : pièces jointes puis sélecteur de fichiers.
        app.on_key(ctrl('p')).await;
        terminal.draw(|f| app.draw(f)).unwrap();
        assert!(screen(&terminal).contains("Pièces jointes de la conversation"));
        app.on_key(key(KeyCode::Esc)).await;
        app.last_dir = sources.clone();
        app.on_key(ctrl('o')).await;
        for c in "source".chars() {
            app.on_key(key(KeyCode::Char(c))).await;
        }
        terminal.draw(|f| app.draw(f)).unwrap();
        assert!(screen(&terminal).contains("source photo.txt"));
        app.on_key(key(KeyCode::Enter)).await;
        assert!(app.modal.is_none());
        assert_eq!(app.files, vec![file]);

        // Très petits terminaux : pas de panique.
        for (w, h) in [(1, 1), (20, 5), (36, 8), (60, 12)] {
            let mut small = Terminal::new(TestBackend::new(w, h)).unwrap();
            small.draw(|f| app.draw(f)).unwrap();
            app.on_key(ctrl('p')).await;
            small.draw(|f| app.draw(f)).unwrap();
            app.on_key(key(KeyCode::Esc)).await;
        }
        app.on_key(ctrl('q')).await;
        assert!(app.quit);
        drop(rx.try_recv());
    }

    #[tokio::test]
    async fn shows_images_inline() {
        let (mut app, mut rx) = app();
        let path = app.att_dir.join("photo.png");
        image::RgbImage::from_pixel(64, 48, image::Rgb([255, 0, 0])).save(&path).unwrap();
        app.current = Some(app.chats[0].key.clone());
        let mut m = msg(1_700_000_000_000, Uuid::from_bytes([1; 16]), false, "regarde");
        m.atts.push(Att { name: "photo.png".into(), mime: "image/png".into(), size: 1234, path: path.clone(), pointer: None });
        app.msgs = vec![m];
        let mut terminal = Terminal::new(TestBackend::new(110, 30)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        assert!(screen(&terminal).contains("chargement de l'image"));
        let ev = tokio::time::timeout(Duration::from_secs(10), rx.recv()).await.unwrap().unwrap();
        assert!(matches!(&ev, Ev::Image { image: Some(_), .. }));
        app.on_event(ev).await;
        terminal.draw(|f| app.draw(f)).unwrap();
        assert!(screen(&terminal).contains("photo.png"));
        let buffer = terminal.backend().buffer();
        let red = |c: Color| matches!(c, Color::Rgb(r, g, b) if r > 200 && g < 60 && b < 60);
        let painted = buffer.content().iter().filter(|cell| red(cell.fg) || red(cell.bg)).count();
        assert!(painted >= 4, "image absente ({painted} cellules rouges)");
    }

    #[test]
    fn input_editing() {
        let mut input = Input::default();
        input.insert_str("bonjour le monde");
        input.key(ctrl('w'));
        assert_eq!(input.text(), "bonjour le ");
        input.key(key(KeyCode::Home));
        input.key(key(KeyCode::Delete));
        assert_eq!(input.text(), "onjour le ");
        input.set("un texte bien trop long pour la zone");
        let (shown, cursor) = input.view(10);
        assert!(shown.width() <= 10 && cursor < 10, "{shown:?} {cursor}");
        assert_eq!(input.take(), "un texte bien trop long pour la zone");
    }
}
