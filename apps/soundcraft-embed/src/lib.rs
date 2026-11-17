//! SoundCraft hosted inside Workbench's side panel: the same app as the desktop build, drawn into a view
//! Workbench owns, with Workbench's look and only the tracks showing (Hands off) until the user asks for the
//! menus. Workbench (and Claude) drive it through its control channel. See `craft-embed`.
//!
//! The protocol's methods go to SoundCraft's own control channel (`soundcraft_ui_egui::control`), which already
//! has `engine.execute`, `engine.commands`, `document.inspect` and `ui.inspect`. Files are sessions on disk:
//! `file.open {path}` is `session.open` for a session and an import for audio or MIDI, `file.new` is
//! `session.new`, `file.saveAs {path}` is `session.save_as` (replacing the file Workbench saves to), and
//! `file.import {path}` brings an audio file in on a new track at the insertion point, as dropping it does
//! (a MIDI file through `file.import_midi`). SoundCraft always has a session open (an empty one at first),
//! so an import never needs one made for it.
//! Scripted input (`ui.click`, `ui.key`...) replies once a frame has applied it, so those requests answer
//! `pending` and the reply comes from [`EmbedApp::settle`].
//!
//! Playback goes to the default output device (`CRAFT_NO_AUDIO=1`: none, the transport runs silent).
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};

use craft_embed::{EmbedApp, egui};
use serde_json::{Value, json};
use soundcraft_engine::Engine;
use soundcraft_ui_egui::{ControlRequest, Services, SoundApp, control, dialogs};

struct Sound {
    app: SoundApp,
    /// Requests into the app's control channel (its receiver is the app's `control_rx`)
    tx: Sender<ControlRequest>,
    /// The reply to a request the app answers after a frame (scripted input)
    waiting: Option<Receiver<Value>>,
}

impl EmbedApp for Sound {
    fn logic(&mut self, ctx: &egui::Context) {
        self.app.guarded(|app| app.logic(ctx));
    }
    fn raw_input_hook(&mut self, ctx: &egui::Context, raw: &mut egui::RawInput) {
        self.app.raw_input_hook(ctx, raw);
    }
    fn ui(&mut self, ui: &mut egui::Ui) {
        self.app.guarded(|app| app.ui(ui));
    }
    fn control(&mut self, ctx: &egui::Context, request: &str) -> String {
        let req: Value = serde_json::from_str(request).unwrap_or(Value::Null);
        let method = req.get("method").and_then(Value::as_str).unwrap_or("");
        let params = req.get("params").cloned().unwrap_or_else(|| json!({}));
        let (method, mut params) = translate(method, params);
        let command = params.get("command").and_then(Value::as_str).unwrap_or("").to_owned();
        if matches!(command.as_str(), "file.import_audio" | "file.import_midi")
            && let Some(inner) = params.get_mut("params").and_then(Value::as_object_mut)
            && !inner.contains_key("at")
        {
            // at the insertion point (the edit selection's start), like a file dropped on the window
            inner.insert("at".into(), json!(self.app.engine.session().edit.selection.start));
        }
        match method.as_str() {
            "ui.screenshot" => return err("Workbench takes the screenshot of the panel itself"),
            "file.open" => return self.open(&params),
            _ => {}
        }
        if command == "session.save_as"
            && let Some(path) = params.pointer("/params/path").and_then(Value::as_str)
            && is_bundle(path)
        {
            return self.save_bundle(path);
        }
        let (req, rx) = ControlRequest::new(method, params);
        if self.tx.send(req).is_err() {
            return err("the control channel is closed");
        }
        // The app answers control requests in `logic`; drain them now so a request it answers at once has its
        // reply before this returns.
        control::drain(&mut self.app, ctx);
        match rx.try_recv() {
            Ok(v) => v.to_string(),
            Err(TryRecvError::Empty) => {
                self.waiting = Some(rx);
                json!({"ok": true, "result": {"pending": true}}).to_string()
            }
            Err(TryRecvError::Disconnected) => err("the app dropped the request"),
        }
    }
    fn settle(&mut self) -> Option<String> {
        let rx = self.waiting.as_ref()?;
        let out = match rx.try_recv() {
            Ok(v) => v.to_string(),
            Err(TryRecvError::Empty) => return None,
            Err(TryRecvError::Disconnected) => err("the app dropped the request"),
        };
        self.waiting = None;
        Some(out)
    }
    fn exit(&mut self) {
        self.app.close_autosave();
    }
}

impl Sound {
    /// `file.open {path}`: a session opens; audio and MIDI files are imported into the current one.
    fn open(&mut self, params: &Value) -> String {
        let Some(path) = params.get("path").and_then(Value::as_str) else {
            return err("file.open needs a path");
        };
        let p = std::path::Path::new(path);
        if !p.exists() {
            return err(format!("no file at {path}"));
        }
        if is_bundle(path) {
            // a one-file session (Workbench's .soundcraft, the web build's .scraftz): Workbench saved what was open
            let bytes = match std::fs::read(p) {
                Ok(b) => b,
                Err(e) => return err(format!("couldn't read {path}: {e}")),
            };
            let name = p.file_name().and_then(std::ffi::OsStr::to_str).unwrap_or("Session.scraftz").to_owned();
            self.app.engine.mark_clean();
            let before = self.app.engine.path.take(); // set again by a successful open
            dialogs::open_session_bytes(&mut self.app, name, bytes);
            if self.app.engine.path.is_none() {
                self.app.engine.path = before;
                return err(format!("couldn't open {path}: {}", self.app.ui.status));
            }
            self.app.engine.path = Some(path.to_owned());
            return json!({"ok": true, "result": {"opened": path, "status": self.app.ui.status}}).to_string();
        }
        let session = p.extension().and_then(std::ffi::OsStr::to_str).is_some_and(|e| e.eq_ignore_ascii_case("scraft")) || p.is_dir();
        if session {
            return match self.app.run("session.open", json!({"path": path})) {
                Ok(v) => json!({"ok": true, "result": {"opened": path, "session": v}}).to_string(),
                Err(e) => err(e),
            };
        }
        dialogs::open_path(&mut self.app, p);
        json!({"ok": true, "result": {"imported": path, "status": self.app.ui.status}}).to_string()
    }
}

impl Sound {
    /// `file.saveAs` to a one-file session: the session and its audio in one bundle at `path`.
    fn save_bundle(&mut self, path: &str) -> String {
        let (bytes, fresh) = match soundcraft_engine::io::session_bundle(self.app.engine.session()) {
            Ok(b) => b,
            Err(e) => return err(e),
        };
        if let Err(e) = std::fs::write(path, &bytes) {
            return err(format!("couldn't write {path}: {e}"));
        }
        *self.app.engine.session_mut() = fresh;
        self.app.engine.path = Some(path.to_owned());
        self.app.engine.mark_clean();
        json!({"ok": true, "result": {"saved": path, "bytes": bytes.len()}}).to_string()
    }
}

/// A one-file session: Workbench's `.soundcraft` or the web build's `.scraftz` bundle.
fn is_bundle(path: &str) -> bool {
    let low = path.to_ascii_lowercase();
    low.ends_with(".soundcraft") || low.ends_with(".scraftz")
}

fn err(e: impl std::fmt::Display) -> String {
    json!({"ok": false, "error": e.to_string()}).to_string()
}

/// The protocol's method and params as SoundCraft's control channel names them. File commands may come as
/// methods or as `engine.execute` commands.
fn translate(method: &str, params: Value) -> (String, Value) {
    let (method, params) = match method {
        "engine.execute" | "command" => {
            let command = params.get("command").or_else(|| params.get("id")).and_then(Value::as_str).unwrap_or("");
            if matches!(command, "file.open" | "file.new" | "file.saveAs" | "file.import") {
                (command.to_owned(), params.get("params").cloned().unwrap_or_else(|| json!({})))
            } else {
                return (method.to_owned(), params);
            }
        }
        other => (other.to_owned(), params),
    };
    match method.as_str() {
        "file.new" => ("engine.execute".into(), json!({"command": "session.new", "params": params})),
        "file.saveAs" => {
            let path = params.get("path").cloned().unwrap_or(Value::Null);
            // Workbench saves over its own file again and again
            ("engine.execute".into(), json!({"command": "session.save_as", "params": {"path": path, "replace": true}}))
        }
        "file.import" => {
            let path = params.get("path").cloned().unwrap_or(Value::Null);
            let midi = path.as_str().map(str::to_ascii_lowercase).is_some_and(|p| p.ends_with(".mid") || p.ends_with(".midi") || p.ends_with(".smf"));
            let mut inner = if midi { json!({"path": path}) } else { json!({"path": path, "new_tracks": true}) };
            if let (Some(at), Some(o)) = (params.get("at"), inner.as_object_mut()) {
                o.insert("at".into(), at.clone());
            }
            let command = if midi { "file.import_midi" } else { "file.import_audio" };
            ("engine.execute".into(), json!({"command": command, "params": inner}))
        }
        _ => (method, params),
    }
}

fn make(_ctx: &egui::Context) -> Box<dyn EmbedApp> {
    let (tx, rx) = channel();
    let engine = Engine::default();
    // sound through the default output (a silent clock without one); CRAFT_NO_AUDIO=1 keeps test runs silent
    let player =
        (std::env::var_os("CRAFT_NO_AUDIO").is_none()).then(|| soundcraft_playback::Player::new(std::sync::Arc::new(engine.session().clone())));
    let mut app = SoundApp::new(engine, player, Services::default()).with_control(rx);
    // Workbench's look and Hands off from the first frame; Workbench sets the mode's accent right after
    let _ = app.run("view.workbench", json!({"on": true}));
    let _ = app.run("view.handsOff", json!({"on": true}));
    Box::new(Sound { app, tx, waiting: None })
}

craft_embed::export_embed!(make);
