//! Parsing the command line, by hand.
//!
//! The surface is seven verbs and five flags, and it is written out rather than
//! derived from a crate for two reasons: the error messages are the whole
//! interface for a headless deployment, and the alternative brings a dependency
//! tree to express what fits in this file. The old build parsed its arguments
//! the same way.
//!
//! ```text
//! lightview <dir>              serve <dir> on 127.x.x.x:<ephemeral>, open a browser
//! lightview --serve <dir>      serve <dir> on 0.0.0.0:<port> over TLS, with pairing
//! lightview tag <dir> --plugin <name> [--filter <expr>]
//! lightview pair               mint a one-time pairing code, and exit
//! lightview devices            list paired devices
//! lightview devices revoke <id>
//! lightview password           set the password, reading it from stdin
//! lightview password --clear
//! lightview cache              show the derived-cache directory and its size
//! lightview cache --prune      evict least-recently-opened galleries
//!
//!   --serve takes --port <n> and --tls-san <addr>...
//!   every mode takes --data-dir <path>
//! ```
//!
//! **There is no `--no-browser`.** The launch URL is always printed, so a
//! headless host does not need a flag to learn its own address — and section
//! 6's verification recipe does not depend on one.

use std::path::PathBuf;

pub const USAGE: &str = "\
lightview — a local media gallery that also serves itself over the LAN

USAGE
  lightview <dir>                        open a gallery locally and launch a browser
  lightview --serve <dir>                serve a gallery over the LAN, with pairing
  lightview tag <dir> --plugin <name>    run a plugin over a gallery and write tags
  lightview pair                         mint a one-time pairing code, and exit
  lightview devices [revoke <id>]        list or revoke paired devices
  lightview password [--clear]           set the gallery password, read from stdin
  lightview cache [--prune]              show or trim the derived-cache directory

OPTIONS
  --port <n>           LAN port for --serve (default: server.toml, then 8443)
  --tls-san <addr>     extra name or address the certificate must cover; repeatable
  --filter <expr>      restrict `tag` to the files a filter query names
  --data-dir <path>    put cache, data and config under one directory
  -h, --help           this message
";

#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    /// `lightview <dir>` — loopback, `Owner`, opens a browser.
    Open { dir: PathBuf },
    /// `lightview --serve <dir>` — LAN, `Device`, TLS, pairing.
    Serve {
        dir: PathBuf,
        port: Option<u16>,
        tls_sans: Vec<String>,
    },
    /// `lightview tag <dir> --plugin <name> [--filter <expr>]`.
    Tag {
        dir: PathBuf,
        plugin: String,
        filter: Option<String>,
    },
    Pair,
    Devices,
    RevokeDevice { id: String },
    SetPassword,
    ClearPassword,
    Cache,
    PruneCache,
    Help,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Invocation {
    pub command: Command,
    /// `--data-dir <path>`, which overrides all three XDG roots with
    /// `<path>/{cache,data,config}`. One line in a compose file replaces the
    /// volume mount the exe-relative layout needed — and one flag gives a test
    /// its own private machine, which is what the two-tagging-machines test in
    /// section 6 needs.
    pub data_dir: Option<PathBuf>,
}

/// Parse `argv[1..]`.
pub fn parse<I, S>(args: I) -> Result<Invocation, String>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let args: Vec<String> = args.into_iter().map(Into::into).collect();

    // `--data-dir` is valid in every mode, so it is lifted out before the verb
    // is decided rather than repeated in each branch.
    let mut data_dir = None;
    let mut rest: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--data-dir" {
            let value = args
                .get(i + 1)
                .ok_or("--data-dir requires a path".to_string())?;
            data_dir = Some(PathBuf::from(value));
            i += 2;
            continue;
        }
        rest.push(args[i].clone());
        i += 1;
    }

    let command = parse_command(&rest)?;
    Ok(Invocation { command, data_dir })
}

fn parse_command(args: &[String]) -> Result<Command, String> {
    let Some(first) = args.first().map(String::as_str) else {
        return Ok(Command::Help);
    };

    match first {
        "-h" | "--help" | "help" => Ok(Command::Help),

        "--serve" => {
            let dir = args
                .get(1)
                .ok_or("--serve requires a gallery directory".to_string())?;
            if dir.starts_with('-') {
                return Err("--serve requires a gallery directory".to_string());
            }
            let mut port = None;
            let mut tls_sans = Vec::new();
            let mut i = 2;
            while i < args.len() {
                match args[i].as_str() {
                    "--port" => {
                        let value = args.get(i + 1).ok_or("--port requires a number")?;
                        port = Some(
                            value
                                .parse()
                                .map_err(|_| format!("--port: {value:?} is not a port number"))?,
                        );
                        i += 2;
                    }
                    "--tls-san" => {
                        let value = args.get(i + 1).ok_or("--tls-san requires an address")?;
                        tls_sans.push(value.clone());
                        i += 2;
                    }
                    other => return Err(format!("unexpected argument: {other}")),
                }
            }
            Ok(Command::Serve {
                dir: PathBuf::from(dir),
                port,
                tls_sans,
            })
        }

        "tag" => {
            let dir = args
                .get(1)
                .ok_or("tag requires a gallery directory".to_string())?;
            let mut plugin = None;
            let mut filter = None;
            let mut i = 2;
            while i < args.len() {
                match args[i].as_str() {
                    "--plugin" => {
                        plugin = Some(
                            args.get(i + 1)
                                .ok_or("--plugin requires a name")?
                                .clone(),
                        );
                        i += 2;
                    }
                    "--filter" => {
                        filter = Some(
                            args.get(i + 1)
                                .ok_or("--filter requires a query")?
                                .clone(),
                        );
                        i += 2;
                    }
                    other => return Err(format!("unexpected argument: {other}")),
                }
            }
            Ok(Command::Tag {
                dir: PathBuf::from(dir),
                plugin: plugin.ok_or("tag requires --plugin <name>".to_string())?,
                filter,
            })
        }

        "pair" => expect_no_more(args, Command::Pair),

        "devices" => match args.get(1).map(String::as_str) {
            None => Ok(Command::Devices),
            Some("revoke") => {
                let id = args
                    .get(2)
                    .ok_or("devices revoke requires a device id".to_string())?;
                Ok(Command::RevokeDevice { id: id.clone() })
            }
            Some(other) => Err(format!("unexpected argument: {other}")),
        },

        "password" => match args.get(1).map(String::as_str) {
            None => Ok(Command::SetPassword),
            Some("--clear") => Ok(Command::ClearPassword),
            Some(other) => Err(format!("unexpected argument: {other}")),
        },

        "cache" => match args.get(1).map(String::as_str) {
            None => Ok(Command::Cache),
            Some("--prune") => Ok(Command::PruneCache),
            Some(other) => Err(format!("unexpected argument: {other}")),
        },

        // Anything else is a directory. Reject an unknown flag rather than
        // treating it as a path, which would produce a baffling "no such
        // directory: --srve".
        other if other.starts_with('-') => Err(format!("unknown option: {other}")),
        dir => expect_no_more(
            args,
            Command::Open {
                dir: PathBuf::from(dir),
            },
        ),
    }
}

fn expect_no_more(args: &[String], command: Command) -> Result<Command, String> {
    match args.get(1) {
        None => Ok(command),
        Some(extra) => Err(format!("unexpected argument: {extra}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(args: &[&str]) -> Command {
        parse(args.iter().copied()).expect("parse").command
    }

    #[test]
    fn a_bare_directory_opens_it_locally() {
        assert_eq!(
            parsed(&["/mnt/nas/photos"]),
            Command::Open {
                dir: PathBuf::from("/mnt/nas/photos")
            }
        );
    }

    #[test]
    fn serve_takes_a_port_and_repeatable_sans() {
        assert_eq!(
            parsed(&[
                "--serve",
                "/photos",
                "--port",
                "9000",
                "--tls-san",
                "nas.local",
                "--tls-san",
                "192.168.1.10",
            ]),
            Command::Serve {
                dir: PathBuf::from("/photos"),
                port: Some(9000),
                tls_sans: vec!["nas.local".into(), "192.168.1.10".into()],
            }
        );
    }

    #[test]
    fn data_dir_is_valid_in_every_mode() {
        // The flag every mode takes, lifted out before the verb is decided —
        // and the thing that lets one test process be two machines.
        for args in [
            vec!["--data-dir", "/tmp/a", "/photos"],
            vec!["/photos", "--data-dir", "/tmp/a"],
            vec!["--data-dir", "/tmp/a", "pair"],
            vec!["cache", "--prune", "--data-dir", "/tmp/a"],
        ] {
            let invocation = parse(args.iter().copied()).expect("parse");
            assert_eq!(invocation.data_dir, Some(PathBuf::from("/tmp/a")));
        }
    }

    #[test]
    fn the_verbs_parse() {
        assert_eq!(parsed(&["pair"]), Command::Pair);
        assert_eq!(parsed(&["devices"]), Command::Devices);
        assert_eq!(
            parsed(&["devices", "revoke", "abc123"]),
            Command::RevokeDevice {
                id: "abc123".into()
            }
        );
        assert_eq!(parsed(&["password"]), Command::SetPassword);
        assert_eq!(parsed(&["password", "--clear"]), Command::ClearPassword);
        assert_eq!(parsed(&["cache"]), Command::Cache);
        assert_eq!(parsed(&["cache", "--prune"]), Command::PruneCache);
        assert_eq!(parsed(&[]), Command::Help);
        assert_eq!(parsed(&["--help"]), Command::Help);
    }

    #[test]
    fn tag_requires_a_plugin_and_accepts_a_filter() {
        assert_eq!(
            parsed(&[
                "tag",
                "/photos",
                "--plugin",
                "wd-tagger",
                "--filter",
                "not has::plugin.wd"
            ]),
            Command::Tag {
                dir: PathBuf::from("/photos"),
                plugin: "wd-tagger".into(),
                filter: Some("not has::plugin.wd".into()),
            }
        );
        assert!(parse(["tag", "/photos"]).is_err());
    }

    #[test]
    fn a_mistyped_flag_is_an_error_rather_than_a_directory() {
        // Otherwise `lightview --srve /photos` reports "no such directory:
        // --srve", which sends the reader looking in the wrong place.
        assert!(parse(["--srve", "/photos"]).is_err());
        assert!(parse(["-x"]).is_err());
        assert!(parse(["--serve"]).is_err());
        assert!(parse(["--serve", "--port"]).is_err());
        assert!(parse(["--serve", "/photos", "--port", "notanumber"]).is_err());
        assert!(parse(["--data-dir"]).is_err());
        assert!(parse(["/photos", "extra"]).is_err());
    }

    #[test]
    fn there_is_no_no_browser_flag() {
        // The URL is always printed, so a headless host needs no flag to learn
        // its own address — and the verification recipe cannot come to depend
        // on one that was never defined.
        assert!(!USAGE.contains("--no-browser"));
        assert!(parse(["/photos", "--no-browser"]).is_err());
    }
}
