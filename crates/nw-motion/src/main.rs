//! `nw-motion`: the motion detection plugin daemon. NeuroWatch connects to
//! it and sends bound cameras' video; it answers with motion events.
use nw_analytics_protocol::address::Address;
use nw_motion::{access, server, worker::Options};
use std::path::{Path, PathBuf};

const USAGE: &str = "\
nw-motion: NeuroWatch motion detection plugin

Usage: nw-motion [--listen ADDRESS] [--identity PATH] [--access-codes PATH]
       nw-motion access-codes add|limit|remove|list PATH …   (see access-codes --help)

  --listen ADDRESS      unix:/path, pipe:\\\\.\\pipe\\NAME, tcp:127.0.0.1:PORT,
                        quic:HOST:PORT or tls:HOST:PORT (encrypted TCP fallback)
                        (default: private local Unix socket or Windows pipe)
  --identity PATH       persistent private signing key (keep across restarts)
  --access-codes PATH   serve only recorders sending one of the codes in this
                        file, each to its limit (written by `access-codes`;
                        re-read every 2 s; default: every approved recorder)
  --check               check that GStreamer has every element analysis needs
  --version             print the version
  RUST_LOG              log filter (default: info)";

const ACCESS_USAGE: &str = "\
nw-motion access-codes: the codes recorders must send (as the plugin's vendor
credential in NeuroWatch: Plugins → Vendor Credential…). The file keeps only
salted PBKDF2 hashes; a running daemon applies changes within 2 s.

  add PATH NAME --max-connections N [--code-stdin]
        add a code (or give NAME a new one, which disconnects recorders using
        the old one); prints a new random code once, or reads yours from stdin
  limit PATH NAME N     change how many recorders NAME admits at once
  remove PATH NAME      remove a code (its recorders are disconnected)
  list PATH             names and limits (codes cannot be shown)";

fn main() -> anyhow::Result<()> {
    use_release_gstreamer();
    if std::env::args().nth(1).as_deref() == Some("access-codes") {
        return access_codes(std::env::args().skip(2).collect());
    }
    let mut listen = None;
    let options = Options::default();
    let mut parent = None;
    let mut identity = None;
    let mut access = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--listen" => {
                listen = Some(
                    args.next()
                        .ok_or_else(|| anyhow::anyhow!("--listen needs an address"))?,
                )
            }
            "--identity" => {
                identity = Some(std::path::PathBuf::from(
                    args.next()
                        .ok_or_else(|| anyhow::anyhow!("--identity needs a path"))?,
                ))
            }
            "--access-codes" => {
                access =
                    Some(std::path::PathBuf::from(args.next().ok_or_else(|| {
                        anyhow::anyhow!("--access-codes needs a path")
                    })?))
            }
            "--hardware-decoding" => eprintln!(
                "nw-motion: --hardware-decoding is ignored: choose each camera's Video decoder in its settings"
            ),
            "--parent-pid" => {
                parent = Some(
                    args.next()
                        .ok_or_else(|| anyhow::anyhow!("--parent-pid needs a PID"))?
                        .parse::<u32>()?,
                )
            }
            "--check" => return check(),
            "--version" => {
                println!("nw-motion {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            other => anyhow::bail!("unknown argument {other:?}\n\n{USAGE}"),
        }
    }
    use_bundled_gstreamer();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    // A list that cannot be read stops the daemon: never serve everyone instead.
    let access = std::sync::Arc::new(match access {
        Some(path) => access::Access::load(&path)?,
        None => access::Access::default(),
    });
    access.watch()?;
    gstreamer::init()?;
    if let Some(parent) = parent {
        watch_parent(parent)?;
    }
    let address = Address::parse(&listen.unwrap_or_else(default_address))?;
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        access_codes = access.restricted(),
        "nw-motion starting"
    );
    let identity = identity.unwrap_or_else(|| {
        std::env::var_os("XDG_STATE_HOME")
            .or_else(|| std::env::var_os("LOCALAPPDATA"))
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                std::env::var_os("HOME")
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(std::env::temp_dir)
                    .join(".local/state")
            })
            .join("neurowatch/plugins/nw-motion.key")
    });
    let key = nw_analytics_sdk::secrets::signing_key(&identity)?;
    // As NeuroWatch's Plugins page shows it, to compare before approving.
    let public = nw_analytics_sdk::identity::public_key(&key);
    if let Some(fingerprint) = nw_analytics_sdk::identity::fingerprint(&public) {
        let grouped: Vec<&str> = fingerprint
            .as_bytes()
            .chunks(8)
            .filter_map(|group| std::str::from_utf8(group).ok())
            .collect();
        tracing::info!(fingerprint = %grouped.join(" "), "Plugin key");
    }
    server::listen_with_identity(&address, options, std::sync::Arc::new(key), access)
}

/// `--check`: the GStreamer in use and whether it can run the analysis.
fn check() -> anyhow::Result<()> {
    use_bundled_gstreamer();
    gstreamer::init()?;
    println!("nw-motion {}", env!("CARGO_PKG_VERSION"));
    println!("{}", gstreamer::version_string());
    let missing = nw_motion::worker::missing_elements();
    if !missing.is_empty() {
        anyhow::bail!("GStreamer elements missing: {}", missing.join(", "));
    }
    println!("All required elements found");
    let hardware = nw_motion::worker::hardware_decoders();
    println!(
        "Hardware decoders: {}",
        if hardware.is_empty() {
            "none".to_owned()
        } else {
            hardware.join(", ")
        }
    );
    if let Some(reason) = nw_motion::worker::va_blocked() {
        println!("VA-API not used: {reason}");
    }
    Ok(())
}

/// Started by hand from a NeuroWatch checkout, run on its release GStreamer
/// SDK (`.cache/gstreamer-sdk-<version>`) rather than the system's, as
/// NeuroWatch runs its own backend: Ubuntu 24.04's 1.24.2 logs
/// `gst_mini_object_unref: assertion 'mini_object != NULL' failed` for every
/// picture the analysis rate drops, and its VA-API leaks. GStreamer's libraries
/// load at start, so this starts the process again with them
/// (`gstreamer_runtime`). A release bundle, a copy NeuroWatch runs (it sets the
/// plugin path) and `NW_GSTREAMER_SDK=0` keep what they have.
fn use_release_gstreamer() {
    #[cfg(unix)]
    {
        use nw_motion::gstreamer_runtime::{choose, environment, Choice};
        use std::os::unix::process::CommandExt;
        let Some(exe) = std::env::current_exe()
            .ok()
            .and_then(|e| e.canonicalize().ok())
        else {
            return;
        };
        let chosen = choose(
            &exe,
            std::env::var_os("GST_PLUGIN_SYSTEM_PATH_1_0").as_deref(),
            std::env::var_os("NW_GSTREAMER_SDK").as_deref(),
        );
        let Choice::Use(sdk) = chosen else {
            return;
        };
        let cache = std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
            .unwrap_or_else(std::env::temp_dir)
            .join("nw-motion");
        if let Err(error) = std::fs::create_dir_all(&cache) {
            eprintln!("nw-motion: {error}: staying on the system's GStreamer");
            return;
        }
        let mut again = std::process::Command::new(&exe);
        again.args(std::env::args_os().skip(1));
        let library_path = std::env::var_os("LD_LIBRARY_PATH");
        for (name, value) in environment(&sdk, &cache, library_path.as_deref()) {
            again.env(name, value);
        }
        // Only returns if it failed: carry on where we are.
        let error = again.exec();
        eprintln!("nw-motion: could not start on {}: {error}", sdk.display());
    }
}

/// A release bundle carries its own GStreamer (`gstreamer/lib/gstreamer-1.0`
/// beside `bin/`, as NeuroWatch's own packages do): use only its plugins,
/// with a registry of their own. A variable the environment already sets
/// wins (NeuroWatch sets them for the copy it runs). Called before any
/// other thread starts.
fn use_bundled_gstreamer() {
    let root = std::env::current_exe()
        .and_then(|exe| exe.canonicalize())
        .ok()
        .and_then(|exe| Some(exe.parent()?.parent()?.to_path_buf()));
    let Some(root) = root else {
        return;
    };
    let plugins = root.join("gstreamer").join("lib").join("gstreamer-1.0");
    if !plugins.is_dir() {
        return;
    }
    let cache = std::env::var_os(if cfg!(windows) {
        "LOCALAPPDATA"
    } else {
        "XDG_CACHE_HOME"
    })
    .map(PathBuf::from)
    .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
    .unwrap_or_else(std::env::temp_dir);
    let mut settings = vec![
        (
            "GST_PLUGIN_SYSTEM_PATH_1_0",
            plugins.clone().into_os_string(),
        ),
        ("GST_PLUGIN_PATH_1_0", plugins.into_os_string()),
        (
            "GST_REGISTRY_1_0",
            cache
                .join("nw-motion")
                .join("gstreamer-registry.bin")
                .into_os_string(),
        ),
    ];
    // No plugin scanner ships: scan in this process.
    if std::env::var_os("GST_PLUGIN_SCANNER_1_0").is_none() {
        settings.push(("GST_REGISTRY_FORK", "no".into()));
    }
    for (name, value) in settings {
        if std::env::var_os(name).is_none() {
            std::env::set_var(name, value);
        }
    }
}

/// A crashed recorder must not leave an orphan detector behind. Normal
/// shutdown is also handled by the recorder's owned-child process handle.
fn watch_parent(parent: u32) -> std::io::Result<()> {
    #[cfg(unix)]
    if unsafe { libc::getppid() } as u32 != parent {
        return Err(std::io::Error::other("parent process changed"));
    }
    #[cfg(windows)]
    let handle = {
        use std::os::windows::io::{FromRawHandle, OwnedHandle};
        let handle = unsafe {
            windows_sys::Win32::System::Threading::OpenProcess(
                windows_sys::Win32::System::Threading::SYNCHRONIZE,
                0,
                parent,
            )
        };
        if handle.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        unsafe { OwnedHandle::from_raw_handle(handle) }
    };
    std::thread::Builder::new()
        .name("parent-watch".into())
        .spawn(move || {
            #[cfg(unix)]
            loop {
                std::thread::sleep(std::time::Duration::from_secs(1));
                if unsafe { libc::getppid() } as u32 != parent {
                    std::process::exit(0);
                }
            }
            #[cfg(windows)]
            {
                use std::os::windows::io::AsRawHandle;
                unsafe {
                    windows_sys::Win32::System::Threading::WaitForSingleObject(
                        handle.as_raw_handle(),
                        u32::MAX,
                    );
                }
                std::process::exit(0);
            }
        })
        .map(|_| ())
}

/// Beside NeuroWatch's own socket (`$XDG_RUNTIME_DIR/neurowatch`).
fn default_address() -> String {
    if cfg!(unix) {
        let runtime = std::env::var("XDG_RUNTIME_DIR")
            .ok()
            .filter(|dir| !dir.is_empty())
            .unwrap_or_else(|| "/tmp".into());
        format!("unix:{runtime}/neurowatch/plugins/nw-motion.sock")
    } else {
        r"pipe:\\.\pipe\neurowatch-motion".into()
    }
}

fn access_codes(args: Vec<String>) -> anyhow::Result<()> {
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let open = |path: &str| -> anyhow::Result<access::CodeFile> {
        access::CodeFile::read(Path::new(path))
    };
    match args.as_slice() {
        ["add", path, name, rest @ ..] => {
            let mut max = None;
            let mut from_stdin = false;
            let mut rest = rest.iter();
            while let Some(arg) = rest.next() {
                match *arg {
                    "--max-connections" => {
                        max = Some(
                            rest.next()
                                .ok_or_else(|| anyhow::anyhow!("--max-connections needs a number"))?
                                .parse::<usize>()?,
                        )
                    }
                    "--code-stdin" => from_stdin = true,
                    other => anyhow::bail!("unknown argument {other:?}\n\n{ACCESS_USAGE}"),
                }
            }
            let max = max.ok_or_else(|| anyhow::anyhow!("add needs --max-connections N"))?;
            let mut file = match Path::new(path).exists() {
                true => open(path)?,
                false => access::CodeFile::new()?,
            };
            let code = if from_stdin {
                let mut line = String::new();
                std::io::stdin().read_line(&mut line)?;
                line.trim_end_matches(['\r', '\n']).to_owned()
            } else {
                access::new_code()?
            };
            access::check_code(&code).map_err(anyhow::Error::msg)?;
            file.add(name, &code, max).map_err(anyhow::Error::msg)?;
            file.write(Path::new(path))?;
            if from_stdin {
                println!("Added {name} (up to {max} at once).");
            } else {
                println!("Code for {name} (up to {max} at once). It is shown only now:\n{code}");
            }
        }
        ["limit", path, name, max] => {
            let max = max.parse::<usize>()?;
            let mut file = open(path)?;
            anyhow::ensure!(file.set_limit(name, max), "no code named {name:?}");
            file.write(Path::new(path))?;
            println!("{name} now admits up to {max} at once.");
        }
        ["remove", path, name] => {
            let mut file = open(path)?;
            anyhow::ensure!(file.remove(name), "no code named {name:?}");
            file.write(Path::new(path))?;
            println!("Removed {name}.");
        }
        ["list", path] => {
            for code in open(path)?.codes {
                println!("{}\t{}", code.name, code.max_connections);
            }
        }
        ["-h" | "--help"] | [] => println!("{ACCESS_USAGE}"),
        _ => anyhow::bail!("{ACCESS_USAGE}"),
    }
    Ok(())
}
