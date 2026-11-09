// Kiln's JVM: Java in a process of its own, beside the game's. The game's code forbids `unsafe`
// (`#![forbid(unsafe_code)]` in horizon-client), so a native mod can't load a JVM into the game's process
// through JNI; Kiln runs `java kiln.Host` instead and talks to it over its stdin and stdout, a command a
// line (kiln/java/kiln/Host.java has the protocol). The window's game never waits on Java: one thread writes
// the commands from a bounded queue, another reads the replies into a channel the game drains each tick, so a
// slow or stuck mod never stalls the game. A mod stuck for longer than the queue holds stops Java.
//
// The integrated server's game asks Java things and waits for the answer (`ServerLink::call`), at most a
// timeout: its answers (`return`) come on a channel of their own, and so do Java's questions about the world while a
// call runs (`ask`), which the waiting server answers.

use std::ffi::OsString;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::mpsc::{
    Receiver, RecvTimeoutError, SyncSender, TrySendError, channel, sync_channel,
};
use std::time::{Duration, Instant};

/// Commands waiting for Java to read them: about a minute of ticks. A pipe holds only 64 KiB and a write
/// to a full one blocks, so the game never writes to Java's stdin itself.
pub const QUEUE: usize = 1200;

/// The game's prepared resources folder, as the window said (`set_resources`); none in tests.
static RESOURCES: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

/// The window's game: where its prepared resources are, for Java started after.
pub fn set_resources(path: &Path) {
    if let Ok(mut r) = RESOURCES.lock()
        && r.as_deref() != Some(path)
    {
        *r = Some(path.to_path_buf());
    }
}

/// `-Dkiln.resources=<folder>`, empty when the window hasn't said.
fn resources_property() -> OsString {
    let mut property = OsString::from("-Dkiln.resources=");
    if let Some(path) = RESOURCES.lock().ok().and_then(|r| r.clone()) {
        property.push(path);
    }
    property
}

/// What Java said.
#[derive(Clone, Debug, PartialEq)]
pub enum Reply {
    /// It started: its version.
    Ready(String),
    /// A mod's entrypoint class ran.
    Loaded(String),
    /// A mod's entrypoint class didn't run, and why.
    Failed(String, String),
    /// What the Java mods draw on the HUD changed: all of it, in order.
    Draw(Vec<HudOp>),
    /// A line for the game's log.
    Log(String),
    /// A line for the player's chat.
    Chat(String),
    /// A mod's line for the player's chat as vanilla's JSON, with its style (colour, bold, italic...).
    ChatComponent(serde_json::Value),
    /// The mods' items with a use of their own: all of them, when that changed.
    Items(Vec<String>),
    /// The server events the mods hear (`join`, `respawn`): all of them, when that changed.
    Listens(Vec<String>),
    /// The pools the mods add to a loot table (its id, the pools as the loot table format's JSON array).
    Loot(String, String),
    /// The answer to a server call.
    Return(Return),
    /// During a server call (its number), Java asks the game something (`block <x> <y> <z>`).
    Ask(u64, Vec<String>),
    /// The mods' blocks with code of their own, each `<id> <what>[,<what>]`: all of them, when that changed.
    Blocks(Vec<String>),
    /// The client commands the mods registered (`ClientCommandRegistrationCallback`), by name: all of them, when
    /// that changed.
    Commands(Vec<String>),
    /// The keys the mods registered (`KeyMappingHelper.registerKeyMapping`) since Java last said: the entrypoint
    /// class that registered them (empty for keys registered later), then each key's name, its group's name, its label
    /// and the key the mod set (`key.keyboard.v`). An entrypoint with none says so too.
    Keys(String, Vec<[String; 4]>),
    /// A command a mod sends as the window's player (`ClientPacketListener.sendCommand`), without its slash:
    /// attended (typed, signed) or not.
    Send(bool, String),
    /// A sound, particle or toast a mod played for the window's player.
    Play(Play),
    /// A mod's payload for the server (`ClientPlayNetworking.send`): the message for Horizon's channel for mods, its
    /// type's id then the payload (kiln/java/kiln/Net.java).
    Net(Vec<u8>),
    /// A command was done (its name).
    Done(String),
    /// The Java process ended: why.
    Exited(String),
}

/// What a mod plays for the window's player, for the game to play with the next tick.
#[derive(Clone, Debug, PartialEq)]
pub enum Play {
    /// A sound by its id (`minecraft:ui.button.click`): at a place in the level, or the player's own (the UI's)
    /// without one.
    Sound {
        id: String,
        at: Option<[f64; 3]>,
        volume: f32,
        pitch: f32,
    },
    /// A particle by its type's id (`minecraft:happy_villager`), at a place, moving.
    Particle {
        id: String,
        at: [f64; 3],
        velocity: [f64; 3],
    },
    /// A system toast: its title, its message (none when empty), how long it shows in milliseconds, and what it is
    /// about (its token): to `replace`, one of that token still shown takes the title and message instead.
    Toast {
        title: String,
        message: String,
        millis: f64,
        token: String,
        replace: bool,
    },
}

/// Java's answer to a server call: the call's number, the result and what the mod's code did (kiln/java/kiln/Server.java
/// has their forms).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Return {
    pub call: u64,
    pub result: String,
    pub effects: Vec<String>,
}

/// `\`, tab, newline and carriage return escaped, as Host.java reads them.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out
}

pub fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('t') => out.push('\t'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

/// Bytes as lowercase hex, two digits each, as Java's `HexFormat` writes them.
pub fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Hex back into bytes; None for an odd length or a character that isn't a hex digit.
pub fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    let digit = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    text.as_bytes()
        .chunks(2)
        .map(|pair| Some((digit(pair[0])? << 4) | digit(pair[1])?))
        .collect()
}

/// One thing a Java mod drew on the HUD, in GUI pixels (fractions where a mod scaled what it draws); colours are
/// ARGB.
#[derive(Clone, Debug, PartialEq)]
pub enum HudOp {
    Fill {
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        argb: u32,
    },
    /// Text with its top left at (x, y), `scale` times the font's size.
    Text {
        x: f32,
        y: f32,
        scale: f32,
        argb: u32,
        shadow: bool,
        text: String,
    },
    /// An item's icon (`minecraft:diamond_helmet`), `size` pixels square, its top left at (x, y).
    Item {
        x: f32,
        y: f32,
        size: f32,
        item: String,
    },
    /// What is drawn after it is cut to the box from (x0, y0) to (x1, y1) (the GUI scissor), or, with none, not cut.
    Clip(Option<[f32; 4]>),
}

/// A draw op as GuiGraphicsExtractor.java writes it: `f x y w h argb`, `t x y argb shadow text`, `s x y scale argb
/// shadow text` (scaled text), `i x y size item`, or `c x0 y0 x1 y1` (cut what follows to the box) and `c` (stop).
pub fn parse_op(op: &str) -> Option<HudOp> {
    if op == "c" {
        return Some(HudOp::Clip(None));
    }
    let (kind, rest) = op.split_once(' ')?;
    // The text is the last field and may hold spaces.
    let fields = if kind == "s" { 6 } else { 5 };
    let mut parts = rest.splitn(fields, ' ');
    let mut number = || parts.next()?.parse::<f64>().ok();
    match kind {
        "f" => Some(HudOp::Fill {
            x: number()? as f32,
            y: number()? as f32,
            w: number()? as f32,
            h: number()? as f32,
            argb: number()? as u32,
        }),
        "t" | "s" => {
            let (x, y) = (number()? as f32, number()? as f32);
            let scale = if kind == "s" { number()? as f32 } else { 1.0 };
            let (argb, shadow) = (number()? as u32, number()? == 1.0);
            Some(HudOp::Text {
                x,
                y,
                scale,
                argb,
                shadow,
                text: parts.next().unwrap_or_default().to_owned(),
            })
        }
        "i" => {
            let (x, y, size) = (number()? as f32, number()? as f32, number()? as f32);
            let item = parts.next()?.to_owned();
            (!item.is_empty()).then_some(HudOp::Item { x, y, size, item })
        }
        "c" => Some(HudOp::Clip(Some([
            number()? as f32,
            number()? as f32,
            number()? as f32,
            number()? as f32,
        ]))),
        _ => None,
    }
}

/// A reply line: a word and tab-separated, escaped fields.
pub fn parse(line: &str) -> Reply {
    let mut fields = line.split('\t');
    let kind = fields.next().unwrap_or("");
    match kind {
        "draw" => return Reply::Draw(fields.filter_map(|f| parse_op(&unescape(f))).collect()),
        "items" => return Reply::Items(fields.map(unescape).collect()),
        "blocks" => return Reply::Blocks(fields.map(unescape).collect()),
        "ask" => {
            let call = fields.next().and_then(|c| c.parse().ok()).unwrap_or(0);
            return Reply::Ask(call, fields.map(unescape).collect());
        }
        "listens" => return Reply::Listens(fields.map(unescape).collect()),
        "commands" => return Reply::Commands(fields.map(unescape).collect()),
        "keys" => {
            let owner = fields.next().map(unescape).unwrap_or_default();
            let fields: Vec<String> = fields.map(unescape).collect();
            let keys = fields
                .chunks_exact(4)
                .map(|k| [k[0].clone(), k[1].clone(), k[2].clone(), k[3].clone()])
                .collect();
            return Reply::Keys(owner, keys);
        }
        "return" => {
            let call = fields.next().and_then(|c| c.parse().ok()).unwrap_or(0);
            let result = fields.next().map(unescape).unwrap_or_default();
            let effects = fields.map(unescape).collect();
            return Reply::Return(Return {
                call,
                result,
                effects,
            });
        }
        _ => {}
    }
    let mut next = || fields.next().map(unescape).unwrap_or_default();
    match kind {
        "ready" => Reply::Ready(next()),
        "loaded" => Reply::Loaded(next()),
        "failed" => {
            let class = next();
            Reply::Failed(class, next())
        }
        "loot" => {
            let id = next();
            Reply::Loot(id, next())
        }
        "log" => Reply::Log(next()),
        "chat" => Reply::Chat(next()),
        "chat_json" => {
            let json = next();
            match serde_json::from_str(&json) {
                Ok(component) => Reply::ChatComponent(component),
                Err(_) => Reply::Log(format!("unreadable chat component: {json}")),
            }
        }
        "send" => {
            let attended = next() == "1";
            Reply::Send(attended, next())
        }
        // `sound <id> <x y z, or nothing> <volume> <pitch>`, `particle <id> <x y z> <dx dy dz>`, `toast <title>
        // <message> <milliseconds> <token> <replace 0|1>`.
        "sound" => {
            let id = next();
            let at = triple(&next());
            let (volume, pitch) = (next().parse().unwrap_or(1.0), next().parse().unwrap_or(1.0));
            Reply::Play(Play::Sound {
                id,
                at,
                volume,
                pitch,
            })
        }
        "particle" => {
            let id = next();
            match (triple(&next()), triple(&next())) {
                (Some(at), Some(velocity)) => Reply::Play(Play::Particle { id, at, velocity }),
                _ => Reply::Log(format!("unreadable particle: {line}")),
            }
        }
        "toast" => {
            let (title, message) = (next(), next());
            let millis = next().parse().unwrap_or(5000.0);
            let token = next();
            Reply::Play(Play::Toast {
                title,
                message,
                millis,
                token,
                replace: next() == "1",
            })
        }
        "net" => match unhex(&next()) {
            Some(data) => Reply::Net(data),
            None => Reply::Log(format!("unreadable payload: {line}")),
        },
        "done" => Reply::Done(next()),
        _ => Reply::Log(format!("unknown reply: {line}")),
    }
}

/// Three finite numbers apart by spaces (a place, a motion); none otherwise.
fn triple(field: &str) -> Option<[f64; 3]> {
    let mut parts = field.split(' ').map(|n| n.parse::<f64>().ok().filter(|v| v.is_finite()));
    let v = [parts.next()??, parts.next()??, parts.next()??];
    parts.next().is_none().then_some(v)
}

/// The `java` program inside a Java home.
pub fn java_program(home: &Path) -> PathBuf {
    let name = if cfg!(windows) { "java.exe" } else { "java" };
    home.join("bin").join(name)
}

/// Tests: Kiln's side of the "Java" a test just wrote to `home`. On Linux a fork in another test's thread can hold the
/// script open for writing for a moment, and starting it then fails with "Text file busy": it is tried again.
#[cfg(test)]
pub fn start_written(home: &Path) -> JavaHost {
    for _ in 0..100 {
        match JavaHost::start(home, &[], home) {
            Err(e) if e.contains("Text file busy") => std::thread::sleep(Duration::from_millis(10)),
            started => return started.unwrap(),
        }
    }
    panic!("{} stayed busy", java_program(home).display())
}

/// A command line: the command and its escaped fields, tab-separated.
pub fn command_line(command: &str, fields: &[&str]) -> String {
    let mut line = command.to_owned();
    for f in fields {
        line.push('\t');
        line.push_str(&escape(f));
    }
    line
}

/// Queues `line` for Java's stdin; never waits.
fn queue(commands: &SyncSender<String>, line: String) -> Result<(), String> {
    commands.try_send(line).map_err(|e| match e {
        TrySendError::Full(_) => {
            format!("Java hasn't read the last {QUEUE} commands: a mod is stuck")
        }
        TrySendError::Disconnected(_) => "Java isn't listening".to_owned(),
    })
}

/// The Java process, the queue its writer thread takes commands from, the replies its reader thread has
/// collected, and the server's link until the game takes it.
pub struct JavaHost {
    /// The Java process, when Java isn't running inside the game (`halcyon_jvm`).
    child: Option<Child>,
    commands: SyncSender<String>,
    replies: Receiver<Reply>,
    server: Option<ServerLink>,
}

/// The integrated server's way to Java: the same command queue, and the answers to its calls (and Java's questions
/// during them). Any thread may hold it; one call at a time.
pub struct ServerLink {
    commands: SyncSender<String>,
    returns: Receiver<Reply>,
    /// Answers that came after their calls gave up waiting, the newest `LATE_KEPT`, for `take_late`.
    late: Mutex<VecDeque<Return>>,
}

/// How many answers that came too late a link keeps.
pub const LATE_KEPT: usize = 64;

impl ServerLink {
    /// Sends `command` with `fields` (the first is the call's number, `call`) and waits for its answer, up to
    /// `timeout` after the call or Java's last question. Java's questions meanwhile (`ask`) get `answer`'s answers;
    /// answers to earlier calls, which came too late, are kept for `take_late`, and questions from them get `gave_up`,
    /// so Java never waits for a game that gave up, and stops that call rather than go on with no answers.
    pub fn call(
        &self,
        call: u64,
        command: &str,
        fields: &[&str],
        timeout: Duration,
        answer: &mut dyn FnMut(&[String]) -> String,
    ) -> Result<Return, String> {
        queue(&self.commands, command_line(command, fields))?;
        let mut deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.returns.recv_timeout(left) {
                Ok(Reply::Return(answer)) if answer.call == call => return Ok(answer),
                Ok(Reply::Ask(asked, what)) => {
                    let line = if asked == call {
                        command_line("answer", &[&asked.to_string(), &answer(&what)])
                    } else {
                        command_line("gave_up", &[&asked.to_string()])
                    };
                    queue(&self.commands, line)?;
                    deadline = Instant::now() + timeout;
                }
                Ok(Reply::Return(late)) => self.keep_late(late),
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout) => {
                    return Err(format!("no answer from Java in {} ms", timeout.as_millis()));
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err("Java isn't listening".to_owned());
                }
            }
        }
    }
}

impl ServerLink {
    fn keep_late(&self, late: Return) {
        let mut kept = self.late.lock().unwrap_or_else(|e| e.into_inner());
        if kept.len() == LATE_KEPT {
            kept.pop_front();
        }
        kept.push_back(late);
    }

    /// The answers that came after their calls gave up, of the calls `wanted` says, oldest first; never waits. What
    /// came since the last call is read first; a question from a call nobody waits for gets `gave_up`.
    pub fn take_late(&self, wanted: impl Fn(u64) -> bool) -> Vec<Return> {
        while let Ok(reply) = self.returns.try_recv() {
            match reply {
                Reply::Return(late) => self.keep_late(late),
                Reply::Ask(asked, _) => {
                    let _ = queue(&self.commands, command_line("gave_up", &[&asked.to_string()]));
                }
                _ => {}
            }
        }
        let mut kept = self.late.lock().unwrap_or_else(|e| e.into_inner());
        let (taken, left): (Vec<Return>, Vec<Return>) = kept.drain(..).partition(|r| wanted(r.call));
        kept.extend(left);
        taken
    }
}

impl JavaHost {
    /// Starts `java kiln.Host` from the Java home `home`, with `class_path` (Kiln's runtime jar first, then
    /// the libraries' and the mods' jars) and Kiln's folder `dir` (the mods' game folder); its stderr, the mods' own output,
    /// goes to `dir`/java.log.
    pub fn start(home: &Path, class_path: &[PathBuf], dir: &Path) -> Result<JavaHost, String> {
        JavaHost::start_logged(home, class_path, dir, "java.log")
    }

    /// As [`JavaHost::start`], the mods' own output going to `dir`/`log`.
    pub fn start_logged(
        home: &Path,
        class_path: &[PathBuf],
        dir: &Path,
        log: &str,
    ) -> Result<JavaHost, String> {
        // Java runs in `dir`: relative paths would be read from there.
        let absolute = |p: &Path| std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
        let class_path: Vec<PathBuf> = class_path.iter().map(|p| absolute(p)).collect();
        let dir = &absolute(dir);
        let joined = std::env::join_paths(&class_path).map_err(|e| e.to_string())?;
        let log_file = dir.join(log);
        let log =
            std::fs::File::create(&log_file).map_err(|e| format!("{}: {e}", log_file.display()))?;
        // As an OsString, so a game folder whose name isn't UTF-8 reaches Java unchanged.
        let mut kiln_dir = OsString::from("-Dkiln.dir=");
        kiln_dir.push(dir);
        // Kiln's runtime is a Java agent too (kiln.Mixins, Kiln's Mixin): it patches the stand-ins with the mods'
        // mixins as they load. Only for Kiln's runtime jar, named so, which comes first.
        let mut agent = OsString::from("-javaagent:");
        if let Some(runtime) = class_path
            .first()
            .filter(|p| p.file_name() == Some("kiln-runtime.jar".as_ref()))
        {
            agent.push(runtime);
        }
        // Java inside the game's own process (Android, which kills a child process before the app), or as the child
        // process everywhere else: either way, commands go to its stdin and replies come from its stdout.
        let (mut stdin, stdout, child): (
            Box<dyn std::io::Write + Send>,
            Box<dyn std::io::Read + Send>,
            Option<Child>,
        ) = if halcyon_jvm::wanted() {
            let mut options: Vec<OsString> = Vec::new();
            if agent.len() > "-javaagent:".len() {
                options.push(agent.clone());
            }
            options.push("-Xmx512m".into());
            // One collector thread and no parallel heap: this is a phone's memory.
            options.push("-XX:+UseSerialGC".into());
            options.push("-Djoml.nounsafe=true".into());
            options.push(resources_property());
            options.push(kiln_dir.clone());
            let embedded = halcyon_jvm::start(halcyon_jvm::Launch {
                home: home.to_path_buf(),
                options,
                class_path: joined.clone(),
                main_class: "kiln.Host".into(),
                dir: dir.clone(),
                log: log_file.clone(),
            })?;
            (embedded.stdin, embedded.stdout, None)
        } else {
            let mut command = Command::new(java_program(home));
            if agent.len() > "-javaagent:".len() {
                command.arg(agent);
            }
            // In Kiln's folder, as vanilla runs in the game folder: a mod's relative `config/` is FabricLoader's.
            let mut child = command
                .current_dir(dir)
                .arg("-Xmx512m")
                // JOML (the mods' HUD poses) without sun.misc.Unsafe, which Java 25 warns about in the log.
                .arg("-Djoml.nounsafe=true")
                // The game's prepared resources (its language files, its assets), for the mods' text and resources.
                .arg(resources_property())
                .arg(kiln_dir)
                .arg("-cp")
                .arg(joined)
                .arg("kiln.Host")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(log)
                .spawn()
                .map_err(|e| format!("can't run {}: {e}", java_program(home).display()))?;
            let stdin = child.stdin.take().ok_or("no stdin for Java")?;
            let stdout = child.stdout.take().ok_or("no stdout for Java")?;
            (Box::new(stdin), Box::new(stdout), Some(child))
        };
        let (commands, queue) = sync_channel::<String>(QUEUE);
        // Ends when Java stops reading (its stdin breaks) or the game drops the JavaHost.
        std::thread::Builder::new()
            .name("kiln-java-in".into())
            .spawn(move || {
                for line in queue {
                    if writeln!(stdin, "{line}")
                        .and_then(|()| stdin.flush())
                        .is_err()
                    {
                        return;
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        let (sender, replies) = channel();
        let (answers, returns) = channel();
        std::thread::Builder::new()
            .name("kiln-java".into())
            .spawn(move || {
                let mut why = "it closed its output".to_owned();
                for line in BufReader::new(stdout).lines() {
                    match line {
                        // Nobody waiting for an answer any more is no reason to stop.
                        Ok(line) => match parse(&line) {
                            reply @ (Reply::Return(_) | Reply::Ask(..)) => {
                                let _ = answers.send(reply);
                            }
                            reply => {
                                if sender.send(reply).is_err() {
                                    return;
                                }
                            }
                        },
                        Err(e) => {
                            why = e.to_string();
                            break;
                        }
                    }
                }
                let _ = sender.send(Reply::Exited(why));
            })
            .map_err(|e| e.to_string())?;
        let server = Some(ServerLink {
            commands: commands.clone(),
            returns,
            late: Mutex::new(VecDeque::new()),
        });
        Ok(JavaHost {
            child,
            commands,
            replies,
            server,
        })
    }

    /// Queues a command (`load`, `font`, `key`, `adv`, `tick`) with its fields; never waits.
    pub fn send(&mut self, command: &str, fields: &[&str]) -> Result<(), String> {
        queue(&self.commands, command_line(command, fields))
    }

    /// The integrated server's link to this Java, once.
    pub fn take_server_link(&mut self) -> Option<ServerLink> {
        self.server.take()
    }

    /// The replies since the last call, oldest first; never waits.
    pub fn replies(&self) -> Vec<Reply> {
        self.replies.try_iter().collect()
    }
}

impl Drop for JavaHost {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fields_survive_escaping() {
        let text = "a\tb\nc\\d\re";
        assert_eq!(escape(text), "a\\tb\\nc\\\\d\\re");
        assert_eq!(unescape(&escape(text)), text);
        assert_eq!(unescape("trailing\\"), "trailing\\");
    }

    /// A "Java" that never reads its stdin, as a mod stuck in a tick: the game's sends return at once, and
    /// once the queue is full they fail instead of blocking the game.
    #[cfg(unix)]
    #[test]
    fn a_stuck_java_never_blocks_the_game() {
        use std::os::unix::fs::PermissionsExt;
        let home = std::env::temp_dir().join(format!("kiln-stuck-java-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join("bin")).unwrap();
        let program = java_program(&home);
        std::fs::write(&program, "#!/bin/sh\nexec sleep 60\n").unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut java = start_written(&home);
        let line = "x".repeat(200);
        let started = std::time::Instant::now();
        let mut failed = None;
        for i in 0..QUEUE * 10 {
            if let Err(e) = java.send("tick", &[&line]) {
                failed = Some((i, e));
                break;
            }
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        let (sent, why) = failed.expect("the queue should fill");
        // The writer thread takes a few commands into the pipe (64 KiB) before it blocks, then the queue fills.
        assert!(sent >= QUEUE, "{sent}");
        assert!(why.contains("stuck"), "{why}");
        drop(java);
        let _ = std::fs::remove_dir_all(&home);
    }

    /// A "Java" that answers each call twice, an old call's number first: the server's call gets its own
    /// answer, never the other, and the window's replies never see them.
    #[cfg(unix)]
    #[test]
    fn a_call_gets_its_own_answer() {
        use std::os::unix::fs::PermissionsExt;
        let home = std::env::temp_dir().join(format!("kiln-call-java-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join("bin")).unwrap();
        let program = java_program(&home);
        let script = "#!/bin/sh\nwhile IFS=\"$(printf '\\t')\" read -r command call rest; do\n  \
                      printf 'return\\t0\\tpass\\n'\n  \
                      printf 'return\\t%s\\tsuccess\\thealth 24.0\\n' \"$call\"\n  \
                      printf 'done\\t%s\\n' \"$command\"\ndone\n";
        std::fs::write(&program, script).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut java = start_written(&home);
        let link = java.take_server_link().unwrap();
        assert!(java.take_server_link().is_none());
        let timeout = Duration::from_secs(5);
        for call in [3, 4] {
            let answer = link
                .call(call, "use", &[&call.to_string()], timeout, &mut |_| {
                    unreachable!("nothing asked")
                })
                .unwrap();
            assert_eq!(answer.call, call);
            assert_eq!(answer.result, "success");
            assert_eq!(answer.effects, ["health 24.0"]);
        }
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(
            java.replies(),
            [Reply::Done("use".into()), Reply::Done("use".into())]
        );
        drop(java);
        assert!(
            link.call(5, "use", &["5"], timeout, &mut |_| String::new())
                .is_err()
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// Kiln's runtime jar first on the class path makes it Java's agent (Kiln's Mixin); without it, Java gets no
    /// `-javaagent` at all, not an empty one (which Java refuses to start with).
    #[cfg(unix)]
    #[test]
    fn only_kilns_runtime_is_javas_agent() {
        use std::os::unix::fs::PermissionsExt;
        let home = std::env::temp_dir().join(format!("kiln-agent-java-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join("bin")).unwrap();
        let program = java_program(&home);
        // Its arguments, one a line, then it waits for its input to close.
        std::fs::write(&program, "#!/bin/sh\nprintf '%s\\n' \"$@\" > args.txt\nexec cat > /dev/null\n").unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        let args = |class_path: &[PathBuf]| -> Vec<String> {
            let _ = std::fs::remove_file(home.join("args.txt"));
            let java = (0..100)
                .find_map(|_| match JavaHost::start(&home, class_path, &home) {
                    Err(e) if e.contains("Text file busy") => {
                        std::thread::sleep(Duration::from_millis(10));
                        None
                    }
                    started => Some(started.unwrap()),
                })
                .expect("the script stayed busy");
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            let text = loop {
                let text = std::fs::read_to_string(home.join("args.txt")).unwrap_or_default();
                if text.contains("kiln.Host") {
                    break text;
                }
                assert!(std::time::Instant::now() < deadline, "Java got {text:?}");
                std::thread::sleep(Duration::from_millis(5));
            };
            drop(java);
            text.lines().map(str::to_owned).collect()
        };
        let runtime = home.join("kiln-runtime.jar");
        let with = args(&[runtime.clone(), home.join("mod.jar")]);
        assert_eq!(with[0], format!("-javaagent:{}", runtime.display()), "{with:?}");
        let without = args(&[home.join("mod.jar")]);
        assert!(without.iter().all(|a| !a.starts_with("-javaagent")), "{without:?}");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn java_started_after_hears_where_the_games_resources_are() {
        set_resources(Path::new("/games/26.3"));
        assert_eq!(resources_property(), OsString::from("-Dkiln.resources=/games/26.3"));
    }

    /// A "Java" whose call asks about a block as an earlier call (one the game gave up on): it is told so
    /// (`gave_up`), not answered, and the call's own answer comes after.
    #[cfg(unix)]
    #[test]
    fn a_question_from_a_call_given_up_on_is_told_so() {
        use std::os::unix::fs::PermissionsExt;
        let home = std::env::temp_dir().join(format!("kiln-gave-up-java-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join("bin")).unwrap();
        let program = java_program(&home);
        let script = "#!/bin/sh\nwhile IFS=\"$(printf '\\t')\" read -r command a b; do\n  \
                      case \"$command\" in\n    \
                      use_block) call=\"$a\"; printf 'ask\\t1\\tblock\\t1\\t2\\t3\\n' ;;\n    \
                      gave_up) printf 'return\\t%s\\tsuccess\\tgave up %s\\n' \"$call\" \"$a\" ;;\n    \
                      answer) printf 'return\\t%s\\tsuccess\\tanswered\\n' \"$call\" ;;\n  \
                      esac\ndone\n";
        std::fs::write(&program, script).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut java = start_written(&home);
        let link = java.take_server_link().unwrap();
        let answer = link
            .call(2, "use_block", &["2"], Duration::from_secs(5), &mut |_| {
                unreachable!("an earlier call's question is not the game's to answer")
            })
            .unwrap();
        assert_eq!(answer.effects, ["gave up 1"]);
        drop(java);
        let _ = std::fs::remove_dir_all(&home);
    }

    /// A "Java" that asks about a block during each call and returns what it was told: the call's own `answer`
    /// answers it.
    #[cfg(unix)]
    #[test]
    fn java_asks_the_game_during_a_call() {
        use std::os::unix::fs::PermissionsExt;
        let home = std::env::temp_dir().join(format!("kiln-ask-java-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join("bin")).unwrap();
        let program = java_program(&home);
        let script = "#!/bin/sh\nwhile IFS=\"$(printf '\\t')\" read -r command a b; do\n  \
                      case \"$command\" in\n    \
                      use_block) printf 'ask\\t%s\\tblock\\t1\\t2\\t3\\n' \"$a\" ;;\n    \
                      answer) printf 'return\\t%s\\tsuccess\\tgot %s\\n' \"$a\" \"$b\" ;;\n  \
                      esac\ndone\n";
        std::fs::write(&program, script).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut java = start_written(&home);
        let link = java.take_server_link().unwrap();
        let mut asked = Vec::new();
        let answer = link
            .call(
                7,
                "use_block",
                &["7"],
                Duration::from_secs(5),
                &mut |what| {
                    asked.push(what.to_vec());
                    "minecraft:stone".to_owned()
                },
            )
            .unwrap();
        assert_eq!(asked, [["block", "1", "2", "3"]]);
        assert_eq!(answer.effects, ["got minecraft:stone"]);
        drop(java);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn replies_parse() {
        assert_eq!(
            parse("ready\t25.0.2 (Eclipse)"),
            Reply::Ready("25.0.2 (Eclipse)".into())
        );
        assert_eq!(
            parse("draw\tf 1 2 30 40 2013265920\tt 5 6 4294967295 1 Hello\\tworld 1:02.345"),
            Reply::Draw(vec![
                HudOp::Fill {
                    x: 1.0,
                    y: 2.0,
                    w: 30.0,
                    h: 40.0,
                    argb: 0x7800_0000
                },
                HudOp::Text {
                    x: 5.0,
                    y: 6.0,
                    scale: 1.0,
                    argb: 0xFFFF_FFFF,
                    shadow: true,
                    text: "Hello\tworld 1:02.345".into()
                },
            ])
        );
        assert_eq!(parse("draw"), Reply::Draw(Vec::new()));
        assert_eq!(
            parse_op("t 1 2 3 0 "),
            Some(HudOp::Text {
                x: 1.0,
                y: 2.0,
                scale: 1.0,
                argb: 3,
                shadow: false,
                text: String::new()
            })
        );
        // Scaled by a mod's pose: fractions, and a scale before the colour.
        assert_eq!(
            parse_op("s 1.5 2.25 0.75 4278190335 1 50%"),
            Some(HudOp::Text {
                x: 1.5,
                y: 2.25,
                scale: 0.75,
                argb: 0xFF00_00FF,
                shadow: true,
                text: "50%".into()
            })
        );
        assert_eq!(
            parse_op("i 2 3 12 minecraft:diamond_helmet"),
            Some(HudOp::Item {
                x: 2.0,
                y: 3.0,
                size: 12.0,
                item: "minecraft:diamond_helmet".into()
            })
        );
        assert_eq!(parse_op("i 2 3 12 "), None);
        assert_eq!(parse_op("x 1"), None);
        assert_eq!(
            parse("failed\ta.B\tjava.lang.ClassNotFoundException: a.B"),
            Reply::Failed("a.B".into(), "java.lang.ClassNotFoundException: a.B".into())
        );
        assert_eq!(parse("done\ttick"), Reply::Done("tick".into()));
        assert_eq!(parse("loaded\ta.B"), Reply::Loaded("a.B".into()));
        assert_eq!(parse("log\tstarted"), Reply::Log("started".into()));
        assert_eq!(
            parse("items\theart_crystals:heart_crystal\ta:b"),
            Reply::Items(vec!["heart_crystals:heart_crystal".into(), "a:b".into()])
        );
        assert_eq!(parse("items"), Reply::Items(Vec::new()));
        assert_eq!(
            parse("ask\t9\tblock\t1\t-2\t3"),
            Reply::Ask(9, vec!["block".into(), "1".into(), "-2".into(), "3".into()])
        );
        assert_eq!(
            parse("blocks\tmymod:lamp use,step"),
            Reply::Blocks(vec!["mymod:lamp use,step".into()])
        );
        assert_eq!(
            parse("loot\tminecraft:chests/simple_dungeon\t[{\"rolls\":1,\"entries\":[]}]"),
            Reply::Loot(
                "minecraft:chests/simple_dungeon".into(),
                "[{\"rolls\":1,\"entries\":[]}]".into()
            )
        );
        assert_eq!(
            parse("listens\tjoin\trespawn"),
            Reply::Listens(vec!["join".into(), "respawn".into()])
        );
        assert_eq!(
            parse("return\t7\tsuccess\thealth 24.0\toverlay {\"text\":\"a\\tb\"}"),
            Reply::Return(Return {
                call: 7,
                result: "success".into(),
                effects: vec!["health 24.0".into(), "overlay {\"text\":\"a\tb\"}".into()],
            })
        );
        assert_eq!(parse("chat"), Reply::Chat(String::new()));
        // Calc's error (#82): red, as the mod styled it, and the escaped tab of the line back in its text.
        assert_eq!(
            parse(concat!(
                "chat_json\t",
                r#"{"text":"Division by zero\\t!","color":"red","bold":true,"italic":true}"#
            )),
            Reply::ChatComponent(serde_json::json!({
                "text": "Division by zero\t!", "color": "red", "bold": true, "italic": true
            }))
        );
        assert_eq!(
            parse("chat_json\t{not json"),
            Reply::Log("unreadable chat component: {not json".into())
        );
        assert_eq!(
            parse("commands\tkiln_testbench\tcalc"),
            Reply::Commands(vec!["kiln_testbench".into(), "calc".into()])
        );
        // A mod's keys, four fields each, for the entrypoint that registered them; one with none says so.
        assert_eq!(
            parse("keys\tfov.Client\tkey.fovtoggle.toggle\tFOV Toggle\tToggle FOV\tkey.keyboard.v"),
            Reply::Keys(
                "fov.Client".into(),
                vec![[
                    "key.fovtoggle.toggle".into(),
                    "FOV Toggle".into(),
                    "Toggle FOV".into(),
                    "key.keyboard.v".into()
                ]]
            )
        );
        assert_eq!(parse("keys\ta.Main"), Reply::Keys("a.Main".into(), Vec::new()));
        assert_eq!(parse("keys"), Reply::Keys(String::new(), Vec::new()));
        assert_eq!(parse("send\t1\thelp help"), Reply::Send(true, "help help".into()));
        assert_eq!(parse("send\t0\tsay hi"), Reply::Send(false, "say hi".into()));
        assert_eq!(
            parse("sound\tminecraft:ui.button.click\t\t0.25\t1"),
            Reply::Play(Play::Sound {
                id: "minecraft:ui.button.click".into(),
                at: None,
                volume: 0.25,
                pitch: 1.0
            })
        );
        assert_eq!(
            parse("sound\tminecraft:entity.experience_orb.pickup\t1.5 70 -3\t0.2\t1"),
            Reply::Play(Play::Sound {
                id: "minecraft:entity.experience_orb.pickup".into(),
                at: Some([1.5, 70.0, -3.0]),
                volume: 0.2,
                pitch: 1.0
            })
        );
        assert_eq!(
            parse("particle\tminecraft:happy_villager\t1 2 3\t0 0.5 0"),
            Reply::Play(Play::Particle {
                id: "minecraft:happy_villager".into(),
                at: [1.0, 2.0, 3.0],
                velocity: [0.0, 0.5, 0.0]
            })
        );
        assert!(matches!(parse("particle\tminecraft:flame\t1 NaN 3\t0 0 0"), Reply::Log(_)));
        assert_eq!(
            parse("toast\tKiln Testbench\ttoast probe\t10000\t1f2e\t1"),
            Reply::Play(Play::Toast {
                title: "Kiln Testbench".into(),
                message: "toast probe".into(),
                millis: 10000.0,
                token: "1f2e".into(),
                replace: true
            })
        );
        assert!(matches!(parse("what"), Reply::Log(_)));
    }

    #[test]
    fn clip_ops_cut_what_follows() {
        assert_eq!(parse_op("c 4 40 64 90"), Some(HudOp::Clip(Some([4.0, 40.0, 64.0, 90.0]))));
        assert_eq!(parse_op("c"), Some(HudOp::Clip(None)));
        assert_eq!(parse_op("c 4 40"), None);
    }

    #[test]
    fn payloads_go_as_hex() {
        assert_eq!(hex(&[0x00, 0x0f, 0xa5, 0xff]), "000fa5ff");
        assert_eq!(hex(&[]), "");
        assert_eq!(unhex("000fa5ff"), Some(vec![0x00, 0x0f, 0xa5, 0xff]));
        assert_eq!(unhex("A5"), Some(vec![0xa5]), "Java may write capitals too");
        assert_eq!(unhex(""), Some(Vec::new()));
        assert_eq!(unhex("abc"), None, "an odd length");
        assert_eq!(unhex("0g"), None, "not a hex digit");
        assert_eq!(unhex("+1"), None, "a sign isn't a digit");
        let all: Vec<u8> = (0..=255).collect();
        assert_eq!(unhex(&hex(&all)), Some(all));
        assert_eq!(parse("net\t0a6b696c6e3a70696e67"), Reply::Net(b"\x0akiln:ping".to_vec()));
        assert!(matches!(parse("net\tzz"), Reply::Log(_)), "an unreadable payload is logged");
    }
}
