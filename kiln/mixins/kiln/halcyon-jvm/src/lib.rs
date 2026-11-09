//! A JVM inside the game's own process, for Android.
//!
//! Android kills a child process (a `java` the game started) much sooner than the app itself, so Kiln's
//! Java runs in the game's process instead, the way Zalith Launcher 2 and Pojav run Minecraft: a bionic JRE's
//! `libjsig.so` is loaded first (so HotSpot's signal handlers chain with Android's), then `libjli.so`, and the
//! JDK launcher's `JLI_Launch` runs `kiln.Host` on a thread of its own.
//!
//! The game's own crate forbids `unsafe`; this crate is a separate one of the bundle, and has only what the
//! game's lock file has (`libloading`). The game talks to Java the way it does to a child process: commands
//! in on one pipe, replies out on the other; Java opens the pipes as `/proc/self/fd/N` (`kiln.in`, `kiln.out`).
//!
//! `JLI_Launch` and the JRE layout are those of OpenJDK 9+; whether a given Android JRE works is told by the
//! log (`start` returns what failed). Nothing here runs on a computer unless `KILN_EMBED_JVM` is set.
use std::ffi::{CString, OsStr, OsString, c_char, c_int};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// What to run: the JRE, the JVM's options, the class path and the mods' game folder.
pub struct Launch {
    /// The Java home (the folder with `lib/libjli.so`).
    pub home: PathBuf,
    /// JVM options: `-javaagent:..`, `-Xmx512m`, `-D..=..`.
    pub options: Vec<OsString>,
    /// The class path, already joined.
    pub class_path: OsString,
    /// The class whose `main` runs (`kiln.Host`).
    pub main_class: String,
    /// Kiln's game folder: relative paths in Java are read from there (`-Duser.dir`).
    pub dir: PathBuf,
    /// Where the mods' own output (Java's stderr) goes.
    pub log: PathBuf,
}

/// The pipes to Java: commands go to `stdin`, replies come from `stdout`. The ends Java uses are held by the
/// thread that runs the JVM, and closed when it ends: `stdout` then reaches its end, as a child's does.
pub struct Embedded {
    pub stdin: Box<dyn Write + Send>,
    pub stdout: Box<dyn Read + Send>,
}

/// Whether Java should run in this process: always on Android, on request elsewhere (to try it).
pub fn wanted() -> bool {
    cfg!(target_os = "android") || std::env::var_os("KILN_EMBED_JVM").is_some_and(|v| v != "0")
}

type JliLaunch = unsafe extern "C" fn(
    c_int,
    *const *const c_char,
    c_int,
    *const *const c_char,
    c_int,
    *const *const c_char,
    *const c_char,
    *const c_char,
    *const c_char,
    *const c_char,
    u8,
    u8,
    u8,
    i32,
) -> c_int;

fn cstring(s: &OsStr) -> Result<CString, String> {
    CString::new(s.as_bytes()).map_err(|_| format!("{} has a NUL", s.to_string_lossy()))
}

/// Starts the JVM; returns once the thread that runs Java is going.
pub fn start(launch: Launch) -> Result<Embedded, String> {
    let lib = launch.home.join("lib");
    let jli = lib.join("libjli.so");
    if !jli.is_file() {
        return Err(format!(
            "{} not found: Kiln runs Java inside the game, which needs a JRE for Android (bionic)",
            jli.display()
        ));
    }

    // To Java: the game writes, Java reads. From Java: Java writes, the game reads.
    let (java_in, to_java) = std::io::pipe().map_err(|e| e.to_string())?;
    let (from_java, java_out) = std::io::pipe().map_err(|e| e.to_string())?;
    let mut argv: Vec<OsString> = vec![launch.home.join("bin").join("java").into_os_string()];
    argv.extend(launch.options);
    argv.push(OsString::from(format!(
        "-Dkiln.in=/proc/self/fd/{}",
        java_in.as_raw_fd()
    )));
    argv.push(OsString::from(format!(
        "-Dkiln.out=/proc/self/fd/{}",
        java_out.as_raw_fd()
    )));
    let mut err = OsString::from("-Dkiln.err=");
    err.push(&launch.log);
    argv.push(err);
    let mut user_dir = OsString::from("-Duser.dir=");
    user_dir.push(&launch.dir);
    argv.push(user_dir);
    argv.push(OsString::from("-cp"));
    argv.push(launch.class_path);
    argv.push(OsString::from(launch.main_class));

    let c_argv: Vec<CString> = argv.iter().map(|a| cstring(a)).collect::<Result<_, _>>()?;
    let java_home = launch.home.clone();
    let (ready, started) = std::sync::mpsc::channel::<Result<(), String>>();
    std::thread::Builder::new()
        .name("kiln-jvm".into())
        .spawn(move || {
            // Java opens these as /proc/self/fd/N: they stay open until the JVM has ended.
            let _java_ends = (java_in, java_out);
            let result = run_jli(&java_home, &lib, &c_argv);
            // JLI_Launch returns when Java's main has: Kiln has stopped.
            let _ = ready.send(result.map(|_| ()));
        })
        .map_err(|e| e.to_string())?;
    // Failing at once (no libjli, a JRE that won't load) is told; running well sends nothing before the JVM ends.
    if let Ok(Err(e)) = started.recv_timeout(std::time::Duration::from_millis(1500)) {
        return Err(e);
    }

    Ok(Embedded {
        stdin: Box::new(to_java),
        stdout: Box::new(from_java),
    })
}

fn run_jli(home: &Path, lib: &Path, argv: &[CString]) -> Result<c_int, String> {
    use libloading::os::unix::{Library, RTLD_GLOBAL, RTLD_NOW};
    // SAFETY: loading the JRE's own libraries, which run their initializers: the point of this crate.
    unsafe {
        // The signal chaining library first, before libjvm: HotSpot's SIGSEGV (implicit null checks) and
        // Android's handlers then chain (Zalith loads it the same way).
        let jsig = lib.join("server").join("libjsig.so");
        let _jsig = if jsig.is_file() {
            Some(Library::open(Some(&jsig), RTLD_NOW | RTLD_GLOBAL).map_err(|e| e.to_string())?)
        } else {
            None
        };
        let jli = Library::open(Some(lib.join("libjli.so")), RTLD_NOW | RTLD_GLOBAL)
            .map_err(|e| format!("libjli.so: {e}"))?;
        let launch: libloading::os::unix::Symbol<JliLaunch> = jli
            .get(b"JLI_Launch\0")
            .map_err(|e| format!("JLI_Launch: {e}"))?;
        // The JDK launcher finds the JRE from libjli's own place (`GetApplicationHomeFromDll`).
        let ptrs: Vec<*const c_char> = argv.iter().map(|a| a.as_ptr()).collect();
        let progname = CString::new("java").unwrap();
        let version = CString::new("25").unwrap();
        let _ = home;
        let code = launch(
            ptrs.len() as c_int,
            ptrs.as_ptr(),
            0,
            std::ptr::null(),
            0,
            std::ptr::null(),
            version.as_ptr(),
            version.as_ptr(),
            progname.as_ptr(),
            progname.as_ptr(),
            0,
            1,
            0,
            0,
        );
        // The libraries stay loaded: the JVM may still run its shutdown code.
        std::mem::forget(jli);
        std::mem::forget(_jsig);
        Ok(code)
    }
}
