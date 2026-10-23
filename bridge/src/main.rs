//! A language server for GDScript.
//!
//! Godot's language server lives inside the Godot editor and listens on a TCP
//! port: there is no `--lsp` mode and no stdio transport. Zed runs one language
//! server per worktree and speaks to it over stdio, so something has to sit in
//! the middle and *own* the Godot process. This is that something.
//!
//! It is a supervisor, not a pipe. It
//!
//!   * starts `godot --headless --editor --path <project> --lsp-port <port>` for
//!     the project the `initialize` request named, on a port of its own, so two
//!     workspaces can never answer for each other;
//!   * restarts it if it dies, and replays `initialize` and every open document,
//!     so the editor's session survives an engine crash;
//!   * says *why* when it cannot start one -- no binary, project that fails to
//!     load, no free port -- through `window/showMessage`, which the editor shows
//!     to the user. A Zed extension cannot do that: it can only return a command
//!     name, and a command that is not there fails silently.

use std::collections::{HashMap, VecDeque};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// The engine's own default is 6005 and the debug adapter's is 6006, so staying
/// off both means a Godot window the user has open is never in the way -- and is
/// never mistaken for the project this process was asked to serve.
const PORT_FIRST: u16 = 6010;
const PORT_LAST: u16 = 6099;

/// How long the engine gets to scan the project and open its port. A cold scan
/// is seconds; a minute is the point where waiting is worse than saying so.
const START_TIMEOUT: Duration = Duration::from_secs(60);

/// How much of the engine's own output is kept for an error message.
const LOG_LINES: usize = 40;

fn main() {
    let options = match Options::from_args() {
        Ok(options) => options,
        Err(text) => {
            eprintln!("godot-lsp-bridge: {text}");
            std::process::exit(2);
        }
    };
    if let Err(error) = run(options) {
        eprintln!("godot-lsp-bridge: {error}");
        std::process::exit(1);
    }
}

struct Options {
    godot: Option<String>,
    project: Option<PathBuf>,
}

impl Options {
    fn from_args() -> Result<Options, String> {
        let mut options = Options {
            godot: None,
            project: None,
        };
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--godot" => {
                    options.godot = Some(args.next().ok_or("--godot needs a path")?);
                }
                "--project" => {
                    options.project = Some(PathBuf::from(
                        args.next().ok_or("--project needs a directory")?,
                    ));
                }
                "--help" | "-h" => {
                    println!("godot-lsp-bridge [--godot PATH] [--project DIR]");
                    println!();
                    println!("A language server that starts and supervises Godot's own.");
                    println!("Speaks LSP on stdin/stdout; the project normally comes from");
                    println!("the `initialize` request and `--project` only overrides it.");
                    std::process::exit(0);
                }
                other => return Err(format!("unknown argument '{other}'")),
            }
        }
        Ok(options)
    }
}

fn run(options: Options) -> Result<(), String> {
    let stdin = io::stdin();
    let mut input = BufReader::new(stdin.lock());
    let output = Arc::new(Mutex::new(io::stdout()));

    // The first message is the one that names the project, and the engine cannot
    // be started without it, so it is read before anything else happens.
    let first = match read_frame(&mut input).map_err(|error| format!("stdin: {error}"))? {
        Some(body) => body,
        // The editor closed before it asked anything.
        None => return Ok(()),
    };
    let request: Value = serde_json::from_slice(&first)
        .map_err(|error| format!("the first message is not JSON: {error}"))?;
    if request.get("method").and_then(Value::as_str) != Some("initialize") {
        return Err("the first message is not `initialize`".to_string());
    }

    let project = options
        .project
        .clone()
        .or_else(|| project_from(&request))
        .ok_or("`initialize` named no project root")?;
    let params = request
        .get("params")
        .cloned()
        .ok_or("`initialize` carried no params")?;

    let godot = find_godot(options.godot.as_deref())?;
    let engine = Arc::new(Engine::default());
    let session = Arc::new(Mutex::new(Session {
        socket: None,
        init: Some(params),
        initialized: false,
        docs: HashMap::new(),
        replay_ids: Vec::new(),
        next_replay_id: 9_000_001,
    }));

    let socket = match start(&godot, &project, &engine) {
        Ok(socket) => socket,
        Err(error) => {
            // This is the case the old extension answered with silence: the
            // editor is told, in the editor, what is wrong and what to do.
            notify(&output, 1, &error);
            return Err(error);
        }
    };

    // The editor's own `initialize`, forwarded verbatim: the engine has to answer
    // it, because its answer is what the editor drives the session with.
    {
        let mut guard = session.lock().expect("session");
        guard.socket = Some(socket.try_clone().map_err(|error| error.to_string())?);
        if let Some(socket) = guard.socket.as_mut() {
            write_frame(socket, &first).map_err(|error| format!("engine: {error}"))?;
        }
    }
    spawn_reader(
        socket.try_clone().map_err(|error| error.to_string())?,
        session.clone(),
        output.clone(),
    );
    spawn_supervisor(
        godot.clone(),
        project.clone(),
        engine.clone(),
        session.clone(),
        output.clone(),
    );

    // Everything after `initialize` is forwarded as it arrives; the bridge only
    // reads it far enough to keep the document set that a restart needs.
    loop {
        let body = match read_frame(&mut input).map_err(|error| format!("stdin: {error}"))? {
            Some(body) => body,
            None => break,
        };
        match forward(&session, &output, &body) {
            Flow::Forward => {}
            Flow::Stop => break,
        }
    }

    engine.stop();
    Ok(())
}

/// What the engine is told to answer *by itself* on a restart. Responses to these
/// ids are dropped rather than forwarded: the editor never asked.
struct Session {
    socket: Option<TcpStream>,
    init: Option<Value>,
    initialized: bool,
    docs: HashMap<String, Document>,
    replay_ids: Vec<i64>,
    next_replay_id: i64,
}

struct Document {
    language_id: String,
    version: i64,
    text: String,
}

enum Flow {
    Forward,
    Stop,
}

/// Update the state a restart needs, then hand the message to the engine.
fn forward(session: &Arc<Mutex<Session>>, output: &Arc<Mutex<io::Stdout>>, body: &[u8]) -> Flow {
    let message: Value = match serde_json::from_slice(body) {
        Ok(message) => message,
        // Not JSON: pass it through untouched rather than eat it.
        Err(_) => {
            return send(session, body);
        }
    };
    let method = message.get("method").and_then(Value::as_str).unwrap_or("");
    match method {
        "initialized" => {
            session.lock().expect("session").initialized = true;
        }
        "textDocument/didOpen" => {
            if let Some(params) = message.get("params") {
                let uri = params.pointer("/textDocument/uri").and_then(Value::as_str);
                if let Some(uri) = uri {
                    let doc = Document {
                        language_id: params
                            .pointer("/textDocument/languageId")
                            .and_then(Value::as_str)
                            .unwrap_or("gdscript")
                            .to_string(),
                        version: params
                            .pointer("/textDocument/version")
                            .and_then(Value::as_i64)
                            .unwrap_or(0),
                        text: params
                            .pointer("/textDocument/text")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                    };
                    session
                        .lock()
                        .expect("session")
                        .docs
                        .insert(uri.to_string(), doc);
                }
            }
        }
        "textDocument/didChange" => {
            let mut guard = session.lock().expect("session");
            if let Some(params) = message.get("params") {
                if let Some(uri) = params.pointer("/textDocument/uri").and_then(Value::as_str) {
                    let version = params
                        .pointer("/textDocument/version")
                        .and_then(Value::as_i64)
                        .unwrap_or(0);
                    if let Some(doc) = guard.docs.get_mut(uri) {
                        if let Some(changes) =
                            params.get("contentChanges").and_then(Value::as_array)
                        {
                            for change in changes {
                                apply_change(&mut doc.text, change);
                            }
                        }
                        doc.version = version;
                    }
                }
            }
        }
        "textDocument/didClose" => {
            if let Some(uri) = message
                .pointer("/params/textDocument/uri")
                .and_then(Value::as_str)
            {
                session.lock().expect("session").docs.remove(uri);
            }
        }
        "exit" => {
            let _ = send(session, body);
            return Flow::Stop;
        }
        _ => {}
    }
    let _ = output;
    send(session, body)
}

/// Write one message to the engine, if there is one listening right now.
fn send(session: &Arc<Mutex<Session>>, body: &[u8]) -> Flow {
    let mut guard = session.lock().expect("session");
    let Some(socket) = guard.socket.as_mut() else {
        // Mid-restart: the message is dropped. Every one of them is either
        // re-sent by the editor or superseded by the state replayed on connect.
        return Flow::Forward;
    };
    if write_frame(socket, body).is_err() {
        guard.socket = None;
    }
    Flow::Forward
}

/// The engine's own server, supervised.
#[derive(Default)]
struct Engine {
    child: Mutex<Option<Child>>,
    log: Mutex<VecDeque<String>>,
    stopped: AtomicBool,
}

impl Engine {
    fn set(&self, child: Child) {
        *self.child.lock().expect("child") = Some(child);
    }

    fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        if let Some(mut child) = self.child.lock().expect("child").take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    fn take_if_exited(&self) -> bool {
        let mut guard = self.child.lock().expect("child");
        let Some(child) = guard.as_mut() else {
            return true;
        };
        match child.try_wait() {
            Ok(Some(_)) => {
                guard.take();
                true
            }
            Ok(None) => false,
            // A child that cannot be waited on is gone as far as this is concerned.
            Err(_) => {
                guard.take();
                true
            }
        }
    }

    fn note(&self, line: String) {
        let mut log = self.log.lock().expect("log");
        if log.len() == LOG_LINES {
            log.pop_front();
        }
        log.push_back(line);
    }

    fn tail(&self) -> String {
        let log = self.log.lock().expect("log");
        let lines: Vec<&str> = log.iter().map(String::as_str).collect();
        lines.join("\n")
    }
}

/// Start the engine for `project` and hand back the socket its server is on.
fn start(
    godot: &str,
    project: &std::path::Path,
    engine: &Arc<Engine>,
) -> Result<TcpStream, String> {
    let port = free_port()?;
    let mut command = Command::new(godot);
    command
        .arg("--headless")
        .arg("--editor")
        .arg("--path")
        .arg(project)
        .arg("--lsp-port")
        .arg(port.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|error| format!("could not start {godot}: {error}"))?;

    if let Some(stdout) = child.stdout.take() {
        let engine = engine.clone();
        thread::spawn(move || drain(stdout, engine));
    }
    if let Some(stderr) = child.stderr.take() {
        let engine = engine.clone();
        thread::spawn(move || drain(stderr, engine));
    }
    engine.set(child);

    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        if let Ok(socket) = TcpStream::connect(("127.0.0.1", port)) {
            let _ = socket.set_nodelay(true);
            return Ok(socket);
        }
        if engine.take_if_exited() {
            return Err(format!(
                "Godot exited before it opened port {port} for {}.{}",
                project.display(),
                tail_of(engine)
            ));
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "Godot did not open port {port} for {} within {}s.{}",
                project.display(),
                START_TIMEOUT.as_secs(),
                tail_of(engine)
            ));
        }
        thread::sleep(Duration::from_millis(200));
    }
}

/// Keep the engine's output flowing. An undrained pipe fills, and a child whose
/// stderr is full is a child that stops making progress -- so this is for more
/// than the error messages.
fn drain(stream: impl Read + Send + 'static, engine: Arc<Engine>) {
    let reader = BufReader::new(stream);
    for line in reader.lines().map_while(Result::ok) {
        if !line.trim().is_empty() {
            engine.note(line);
        }
    }
}

fn tail_of(engine: &Engine) -> String {
    let tail = engine.tail();
    if tail.trim().is_empty() {
        " Its output was empty.".to_string()
    } else {
        format!("\n{}", tail)
    }
}

/// A restart: a fresh engine, a fresh port, and the session replayed onto it.
fn spawn_supervisor(
    godot: String,
    project: PathBuf,
    engine: Arc<Engine>,
    session: Arc<Mutex<Session>>,
    output: Arc<Mutex<io::Stdout>>,
) {
    thread::spawn(move || {
        let mut backoff = Duration::from_secs(1);
        loop {
            // Wait until the engine this bridge started has gone.
            loop {
                if engine.stopped.load(Ordering::SeqCst) {
                    return;
                }
                if engine.take_if_exited() {
                    break;
                }
                thread::sleep(Duration::from_millis(250));
            }
            if engine.stopped.load(Ordering::SeqCst) {
                return;
            }

            notify(&output, 2, "Godot stopped; restarting the language server.");
            session.lock().expect("session").socket = None;
            loop {
                if engine.stopped.load(Ordering::SeqCst) {
                    return;
                }
                thread::sleep(backoff);
                match start(&godot, &project, &engine) {
                    Ok(socket) => match replay(&session, socket, &output) {
                        Ok(()) => {
                            backoff = Duration::from_secs(1);
                            notify(
                                &output,
                                3,
                                "The GDScript language server is answering again.",
                            );
                            break;
                        }
                        Err(error) => notify(&output, 1, &error),
                    },
                    Err(error) => notify(&output, 1, &error),
                }
                backoff = (backoff * 2).min(Duration::from_secs(10));
            }
        }
    });
}

/// Teach a fresh engine the session it missed.
fn replay(
    session: &Arc<Mutex<Session>>,
    socket: TcpStream,
    output: &Arc<Mutex<io::Stdout>>,
) -> Result<(), String> {
    let mut reader = socket.try_clone().map_err(|error| error.to_string())?;
    {
        let mut guard = session.lock().expect("session");
        let params = match guard.init.clone() {
            Some(params) => params,
            None => return Ok(()),
        };
        let id = guard.next_replay_id;
        guard.next_replay_id += 1;
        guard.replay_ids.push(id);

        let initialize =
            json!({"jsonrpc": "2.0", "id": id, "method": "initialize", "params": params});
        write_frame(&mut reader, initialize.to_string().as_bytes())
            .map_err(|error| format!("engine: {error}"))?;
        if guard.initialized {
            write_frame(
                &mut reader,
                br#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#,
            )
            .map_err(|error| format!("engine: {error}"))?;
        }
        for (uri, doc) in guard.docs.iter() {
            let open = json!({
                "jsonrpc": "2.0",
                "method": "textDocument/didOpen",
                "params": {"textDocument": {
                    "uri": uri,
                    "languageId": doc.language_id,
                    "version": doc.version,
                    "text": doc.text,
                }},
            });
            write_frame(&mut reader, open.to_string().as_bytes())
                .map_err(|error| format!("engine: {error}"))?;
        }
        guard.socket = Some(reader.try_clone().map_err(|error| error.to_string())?);
    }
    spawn_reader(reader, session.clone(), output.clone());
    Ok(())
}

/// Engine to editor: everything the engine says, minus the answers to replays.
fn spawn_reader(socket: TcpStream, session: Arc<Mutex<Session>>, output: Arc<Mutex<io::Stdout>>) {
    thread::spawn(move || {
        let mut reader = BufReader::new(socket);
        loop {
            let body = match read_frame(&mut reader) {
                Ok(Some(body)) => body,
                Ok(None) | Err(_) => return,
            };
            if is_replay_response(&session, &body) {
                continue;
            }
            if write_frame(&mut *output.lock().expect("stdout"), &body).is_err() {
                return;
            }
        }
    });
}

fn is_replay_response(session: &Arc<Mutex<Session>>, body: &[u8]) -> bool {
    let Ok(message) = serde_json::from_slice::<Value>(body) else {
        return false;
    };
    let Some(id) = message.get("id").and_then(Value::as_i64) else {
        return false;
    };
    session.lock().expect("session").replay_ids.contains(&id)
}

fn notify(output: &Arc<Mutex<io::Stdout>>, kind: u8, text: &str) {
    let message = json!({
        "jsonrpc": "2.0",
        "method": "window/showMessage",
        "params": {"type": kind, "message": text},
    });
    let _ = write_frame(
        &mut *output.lock().expect("stdout"),
        message.to_string().as_bytes(),
    );
}

/// A port this bridge can give the engine, away from the editor's own.
fn free_port() -> Result<u16, String> {
    for port in PORT_FIRST..=PORT_LAST {
        if TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return Ok(port);
        }
    }
    Err(format!(
        "no free port in {PORT_FIRST}..{PORT_LAST} for the language server"
    ))
}

fn find_godot(explicit: Option<&str>) -> Result<String, String> {
    if let Some(path) = explicit {
        return Ok(path.to_string());
    }
    if let Ok(path) = std::env::var("GODOT_BIN") {
        if !path.trim().is_empty() {
            return Ok(path);
        }
    }
    for name in ["godot4", "godot", "Godot"] {
        if let Some(path) = which(name) {
            return Ok(path);
        }
    }
    Err("no Godot binary: pass --godot <path> or set GODOT_BIN".to_string())
}

fn which(name: &str) -> Option<String> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        for suffix in ["", ".exe", ".cmd", ".bat"] {
            let candidate = dir.join(format!("{name}{suffix}"));
            if candidate.is_file() {
                return Some(candidate.to_string_lossy().into_owned());
            }
        }
    }
    None
}

fn project_from(request: &Value) -> Option<PathBuf> {
    request
        .pointer("/params/rootUri")
        .and_then(Value::as_str)
        .and_then(uri_to_path)
        .or_else(|| {
            request
                .pointer("/params/rootPath")
                .and_then(Value::as_str)
                .map(PathBuf::from)
        })
}

fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let decoded = percent_decode(rest);
    // `file:///C:/…` on Windows: the leading slash is the URI's, not the path's.
    #[cfg(windows)]
    {
        let trimmed = decoded.strip_prefix('/').unwrap_or(&decoded);
        Some(PathBuf::from(trimmed))
    }
    #[cfg(not(windows))]
    {
        Some(PathBuf::from(decoded))
    }
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let high = (bytes[index + 1] as char).to_digit(16);
            let low = (bytes[index + 2] as char).to_digit(16);
            if let (Some(high), Some(low)) = (high, low) {
                out.push((high * 16 + low) as u8);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ---- the LSP framing -------------------------------------------------------

fn read_frame(reader: &mut impl BufRead) -> io::Result<Option<Vec<u8>>> {
    let mut length: Option<usize> = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }
        if let Some(value) = line.strip_prefix("Content-Length:") {
            length = value.trim().parse().ok();
        }
    }
    let length =
        length.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no Content-Length"))?;
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body)?;
    Ok(Some(body))
}

fn write_frame(writer: &mut impl Write, body: &[u8]) -> io::Result<()> {
    write!(writer, "Content-Length: {}\r\n\r\n", body.len())?;
    writer.write_all(body)?;
    writer.flush()
}

// ---- documents, so a restart does not lose what is open --------------------

/// Apply one `contentChanges` entry: a full replacement when it has no range, an
/// edit when it has one. Positions are UTF-16 units, which is what LSP says they
/// are and what Godot's server sends.
fn apply_change(text: &mut String, change: &Value) {
    let replacement = change.get("text").and_then(Value::as_str).unwrap_or("");
    let Some(range) = change.get("range") else {
        *text = replacement.to_string();
        return;
    };
    let start = range.pointer("/start").map(position).unwrap_or((0, 0));
    let end = range.pointer("/end").map(position).unwrap_or((0, 0));
    let start = position_to_offset(text, start.0, start.1);
    let end = position_to_offset(text, end.0, end.1);
    if start <= end && end <= text.len() {
        text.replace_range(start..end, replacement);
    }
}

fn position(value: &Value) -> (u32, u32) {
    (
        value.get("line").and_then(Value::as_u64).unwrap_or(0) as u32,
        value.get("character").and_then(Value::as_u64).unwrap_or(0) as u32,
    )
}

fn position_to_offset(text: &str, line: u32, character: u32) -> usize {
    let mut start = 0;
    for _ in 0..line {
        match text[start..].find('\n') {
            Some(index) => start += index + 1,
            None => return text.len(),
        }
    }
    let mut units = 0;
    for (index, ch) in text[start..].char_indices() {
        if units >= character {
            return start + index;
        }
        units += ch.len_utf16() as u32;
    }
    text.len()
}
