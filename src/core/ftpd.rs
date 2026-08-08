//! The FTP **server** side of printer emulation — pure command/reply logic.
//!
//! Bambu Studio sends a print in two steps: upload the sliced `.gcode.3mf` to
//! the printer's FTPS store, then `print.project_file` naming what it uploaded.
//! Relaying only the MQTT half therefore gets you a client that can watch and
//! control a print but not start one, so the relay has to answer FTP too.
//!
//! I/O-free, like [`crate::core::mqtt`]: [`FtpSession::handle`] turns one command
//! line into an [`FtpAction`], and the connection layer does the sockets. Which
//! commands exist, what they reply, and how a path resolves are all decided here
//! and tested without a socket.
//!
//! The model is the **printer's own** FTP server, not a general one: the
//! capability table in `docs/protocol.md` is the specification, down to `FEAT`
//! answering `502` and `REST` being unsupported. A client that copes with the
//! printer copes with this.

/// A single FTP reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub code: u16,
    pub text: String,
}

impl Reply {
    pub fn new(code: u16, text: impl Into<String>) -> Self {
        Self {
            code,
            text: text.into(),
        }
    }

    /// The wire form: `code text\r\n`.
    pub fn to_line(&self) -> String {
        format!("{} {}\r\n", self.code, self.text)
    }
}

/// Which flavour of passive mode was asked for — they differ only in the reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PassiveKind {
    /// `PASV` → `227 … (h1,h2,h3,h4,p1,p2)`
    Classic,
    /// `EPSV` → `229 … (|||port|)`
    Extended,
}

/// What the connection layer must do with one command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FtpAction {
    /// Write this and carry on.
    Reply(Reply),
    /// Open a data listener and answer with its address.
    Passive(PassiveKind),
    /// Accept the pending data connection and stream the client's bytes to
    /// `path` on the real printer.
    Store { path: String },
    /// Send the printer's copy of `path` down the data connection.
    Retrieve { path: String },
    /// Send a directory listing. `names_only` distinguishes `NLST` from `LIST`.
    List { path: String, names_only: bool },
    /// Delete `path` on the printer.
    Delete { path: String },
    /// Rename `from` to `to` on the printer (`RNFR` then `RNTO`).
    Rename { from: String, to: String },
    /// Create a directory on the printer.
    MakeDir { path: String },
    /// Remove a directory on the printer.
    RemoveDir { path: String },
    /// The size of `path`, for `SIZE`.
    Size { path: String },
    /// Write this and hang up.
    Close(Reply),
}

/// How far through the login a connection is.
#[derive(Debug, PartialEq, Eq)]
enum Login {
    /// Nothing yet — only USER, QUIT and the handful of commands that need no
    /// account are answered.
    Anonymous,
    /// USER was accepted; waiting for PASS.
    AwaitingPass,
    Authenticated,
}

/// One FTP control connection.
pub struct FtpSession {
    login: Login,
    user_ok: bool,
    cwd: String,
    /// The source path of a rename, held between `RNFR` and `RNTO`.
    rename_from: Option<String>,
    access_code: String,
    user: String,
}

impl FtpSession {
    /// A session that will accept `user` + `access_code`.
    pub fn new(user: impl Into<String>, access_code: impl Into<String>) -> Self {
        Self {
            login: Login::Anonymous,
            user_ok: false,
            cwd: "/".to_string(),
            rename_from: None,
            access_code: access_code.into(),
            user: user.into(),
        }
    }

    /// The greeting to send as soon as the connection is up.
    pub fn greeting() -> Reply {
        Reply::new(220, "bambu-rs printer relay")
    }

    pub fn is_authenticated(&self) -> bool {
        self.login == Login::Authenticated
    }

    pub fn cwd(&self) -> &str {
        &self.cwd
    }

    /// Handle one command line (without its trailing CRLF).
    pub fn handle(&mut self, line: &str) -> FtpAction {
        let line = line.trim_end_matches(['\r', '\n']);
        let (verb, arg) = match line.split_once(' ') {
            Some((v, a)) => (v, a.trim()),
            None => (line, ""),
        };
        let verb = verb.to_ascii_uppercase();

        // Reachable without logging in. Everything else waits for PASS, so a
        // peer that never authenticates can't browse or move files.
        match verb.as_str() {
            "USER" => return self.on_user(arg),
            "PASS" => return self.on_pass(arg),
            "QUIT" => {
                return FtpAction::Close(Reply::new(221, "goodbye"));
            }
            "NOOP" => return ok(200, "NOOP ok"),
            // Implicit FTPS: the whole session is already encrypted. Clients
            // still ask, and refusing would look like a downgrade.
            "PBSZ" => return ok(200, "PBSZ=0"),
            "PROT" => {
                return match arg.to_ascii_uppercase().as_str() {
                    "P" | "" => ok(200, "protection level P"),
                    // The control channel is TLS already; a client asking for a
                    // cleartext data channel has misread the situation.
                    _ => ok(534, "only PROT P is available on an implicit-TLS server"),
                };
            }
            _ => {}
        }

        if self.login != Login::Authenticated {
            return ok(530, "log in first");
        }

        match verb.as_str() {
            "SYST" => ok(215, "UNIX Type: L8"),
            // The printer answers 502 here, and a client that handles that
            // handles this. Advertising features we then have to honour is a
            // promise this relay would rather not make.
            "FEAT" => ok(502, "no-features"),
            "OPTS" => ok(200, "ok"),
            "TYPE" => match arg.to_ascii_uppercase().as_str() {
                "I" | "L 8" | "L8" => ok(200, "type set to I"),
                "A" => ok(200, "type set to A"),
                other => ok(504, format!("unsupported type {other}")),
            },
            "MODE" => match arg.to_ascii_uppercase().as_str() {
                "S" => ok(200, "mode S"),
                other => ok(504, format!("unsupported mode {other}")),
            },
            "STRU" => match arg.to_ascii_uppercase().as_str() {
                "F" => ok(200, "structure F"),
                other => ok(504, format!("unsupported structure {other}")),
            },
            "PWD" | "XPWD" => ok(257, format!("\"{}\" is the current directory", self.cwd)),
            "CWD" | "XCWD" => {
                // Not checked against the printer: a `CWD` that lies is caught
                // by the very next command, and checking would mean a round
                // trip per directory change.
                self.cwd = resolve(&self.cwd, arg);
                ok(250, format!("now in {}", self.cwd))
            }
            "CDUP" | "XCUP" => {
                self.cwd = resolve(&self.cwd, "..");
                ok(250, format!("now in {}", self.cwd))
            }
            "PASV" => FtpAction::Passive(PassiveKind::Classic),
            "EPSV" => FtpAction::Passive(PassiveKind::Extended),
            // Active mode would have the relay dial back into the client, which
            // is the direction firewalls exist to stop. Every modern client
            // falls back to passive.
            "PORT" | "EPRT" => ok(502, "passive mode only: use PASV or EPSV"),
            "LIST" => FtpAction::List {
                path: self.resolve_arg(strip_list_flags(arg)),
                names_only: false,
            },
            "NLST" => FtpAction::List {
                path: self.resolve_arg(strip_list_flags(arg)),
                names_only: true,
            },
            "STOR" | "STOU" if arg.is_empty() => ok(501, "STOR needs a path"),
            "STOR" | "STOU" => FtpAction::Store {
                path: self.resolve_arg(arg),
            },
            "RETR" if arg.is_empty() => ok(501, "RETR needs a path"),
            "RETR" => FtpAction::Retrieve {
                path: self.resolve_arg(arg),
            },
            "DELE" if arg.is_empty() => ok(501, "DELE needs a path"),
            "DELE" => FtpAction::Delete {
                path: self.resolve_arg(arg),
            },
            "SIZE" if arg.is_empty() => ok(501, "SIZE needs a path"),
            "SIZE" => FtpAction::Size {
                path: self.resolve_arg(arg),
            },
            "RNFR" if arg.is_empty() => ok(501, "RNFR needs a path"),
            "RNFR" => {
                self.rename_from = Some(self.resolve_arg(arg));
                ok(350, "ready for RNTO")
            }
            "RNTO" if arg.is_empty() => ok(501, "RNTO needs a path"),
            "RNTO" => match self.rename_from.take() {
                None => ok(503, "RNFR first"),
                Some(from) => FtpAction::Rename {
                    from,
                    to: self.resolve_arg(arg),
                },
            },
            // Uploads are whole-file through a temp file, so there is nothing to
            // resume into. The printer refuses REST too.
            "REST" => ok(502, "no restart: uploads are whole-file"),
            "ABOR" => ok(226, "nothing to abort"),
            "MKD" | "XMKD" if arg.is_empty() => ok(501, "MKD needs a path"),
            "MKD" | "XMKD" => FtpAction::MakeDir {
                path: self.resolve_arg(arg),
            },
            "RMD" | "XRMD" if arg.is_empty() => ok(501, "RMD needs a path"),
            "RMD" | "XRMD" => FtpAction::RemoveDir {
                path: self.resolve_arg(arg),
            },
            other => ok(502, format!("{other} is not implemented")),
        }
    }

    fn on_user(&mut self, arg: &str) -> FtpAction {
        // Remembered, not judged: FTP says the verdict comes after PASS, and
        // saying "no such user" here tells an attacker which half was wrong.
        self.user_ok = arg == self.user;
        self.login = Login::AwaitingPass;
        ok(331, "password required")
    }

    fn on_pass(&mut self, arg: &str) -> FtpAction {
        if self.login != Login::AwaitingPass {
            return ok(503, "USER first");
        }
        if self.user_ok && arg == self.access_code {
            self.login = Login::Authenticated;
            return ok(230, "logged in");
        }
        // Back to the start rather than closing: a client that fat-fingers the
        // code can retry, exactly as it could against the printer.
        self.login = Login::Anonymous;
        self.user_ok = false;
        ok(530, "login incorrect")
    }

    /// Resolve a command argument against the working directory, defaulting to
    /// the working directory itself when the argument is empty.
    fn resolve_arg(&self, arg: &str) -> String {
        if arg.is_empty() {
            self.cwd.clone()
        } else {
            resolve(&self.cwd, arg)
        }
    }
}

fn ok(code: u16, text: impl Into<String>) -> FtpAction {
    FtpAction::Reply(Reply::new(code, text))
}

/// Drop the `ls`-style flags some clients put before a `LIST` path (`-la`).
fn strip_list_flags(arg: &str) -> &str {
    arg.split_whitespace()
        .find(|token| !token.starts_with('-'))
        .unwrap_or("")
}

/// Resolve `arg` against `cwd` into an absolute, normalised path.
///
/// `..` above the root stays at the root: the relay's paths become paths on the
/// printer, and a client must not be able to name anything outside its store by
/// counting `..`s.
pub fn resolve(cwd: &str, arg: &str) -> String {
    let base = if arg.starts_with('/') { "/" } else { cwd };
    let mut parts: Vec<&str> = Vec::new();
    for segment in base.split('/').chain(arg.split('/')) {
        match segment {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    format!("/{}", parts.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const USER: &str = "bblp";
    const CODE: &str = "12345678";

    fn session() -> FtpSession {
        FtpSession::new(USER, CODE)
    }

    fn logged_in() -> FtpSession {
        let mut s = session();
        s.handle("USER bblp");
        s.handle("PASS 12345678");
        assert!(s.is_authenticated());
        s
    }

    fn reply(action: FtpAction) -> Reply {
        match action {
            FtpAction::Reply(r) | FtpAction::Close(r) => r,
            other => panic!("expected a plain reply, got {other:?}"),
        }
    }

    // ---- login ------------------------------------------------------------

    #[test]
    fn the_printers_own_credentials_log_in() {
        let mut s = session();
        assert_eq!(reply(s.handle("USER bblp")).code, 331);
        assert_eq!(reply(s.handle("PASS 12345678")).code, 230);
        assert!(s.is_authenticated());
    }

    #[test]
    fn a_wrong_code_is_refused_and_leaves_the_session_locked() {
        let mut s = session();
        s.handle("USER bblp");
        assert_eq!(reply(s.handle("PASS 87654321")).code, 530);
        assert!(!s.is_authenticated());
        // …and the session is still unusable afterwards.
        assert_eq!(reply(s.handle("LIST")).code, 530);
    }

    #[test]
    fn a_wrong_user_is_only_refused_at_the_password_step() {
        // Saying "no such user" at USER tells an attacker which half was wrong.
        let mut s = session();
        assert_eq!(reply(s.handle("USER root")).code, 331);
        assert_eq!(reply(s.handle("PASS 12345678")).code, 530);
        assert!(!s.is_authenticated());
    }

    #[test]
    fn a_failed_login_can_be_retried() {
        let mut s = session();
        s.handle("USER bblp");
        s.handle("PASS wrong");
        s.handle("USER bblp");
        assert_eq!(reply(s.handle("PASS 12345678")).code, 230);
    }

    #[test]
    fn nothing_useful_works_before_logging_in() {
        // The relay fronts a machine; an anonymous peer gets no file access.
        let mut s = session();
        for command in [
            "LIST",
            "STOR /cache/x.3mf",
            "RETR /cache/x.3mf",
            "PASV",
            "DELE /a",
        ] {
            assert_eq!(
                reply(s.handle(command)).code,
                530,
                "{command} should need a login"
            );
        }
    }

    #[test]
    fn pass_without_user_is_out_of_sequence() {
        assert_eq!(reply(session().handle("PASS 12345678")).code, 503);
    }

    // ---- paths ------------------------------------------------------------

    #[test]
    fn paths_resolve_against_the_working_directory() {
        assert_eq!(resolve("/", "cache"), "/cache");
        assert_eq!(resolve("/cache", "a.3mf"), "/cache/a.3mf");
        assert_eq!(resolve("/cache", "/model/b.3mf"), "/model/b.3mf");
        assert_eq!(resolve("/cache/sub", ".."), "/cache");
        assert_eq!(resolve("/cache", "./a.3mf"), "/cache/a.3mf");
        assert_eq!(resolve("/", ""), "/");
    }

    #[test]
    fn dot_dot_cannot_climb_above_the_root() {
        // These paths become paths on the printer. A client must not be able to
        // name something outside the store by counting `..`s.
        assert_eq!(resolve("/", "../../etc/passwd"), "/etc/passwd");
        assert_eq!(resolve("/cache", "../../../.."), "/");
    }

    #[test]
    fn cwd_moves_and_pwd_reports_it() {
        let mut s = logged_in();
        assert_eq!(reply(s.handle("CWD cache")).code, 250);
        assert_eq!(s.cwd(), "/cache");
        let pwd = reply(s.handle("PWD"));
        assert_eq!(pwd.code, 257);
        assert!(pwd.text.contains("\"/cache\""), "{}", pwd.text);
        assert_eq!(reply(s.handle("CDUP")).code, 250);
        assert_eq!(s.cwd(), "/");
    }

    #[test]
    fn a_relative_upload_lands_in_the_working_directory() {
        // This is exactly what Bambu Studio does: CWD /cache, then STOR by name.
        let mut s = logged_in();
        s.handle("CWD /cache");
        assert_eq!(
            s.handle("STOR plate.gcode.3mf"),
            FtpAction::Store {
                path: "/cache/plate.gcode.3mf".to_string()
            }
        );
    }

    #[test]
    fn a_filename_with_spaces_survives_intact() {
        // Observed on the printer: `Bed scraper by JernejP.gcode.3mf`. Splitting
        // the argument on whitespace would truncate it to "Bed".
        let mut s = logged_in();
        assert_eq!(
            s.handle("STOR /model/Bed scraper by JernejP.gcode.3mf"),
            FtpAction::Store {
                path: "/model/Bed scraper by JernejP.gcode.3mf".to_string()
            }
        );
    }

    // ---- transfers --------------------------------------------------------

    #[test]
    fn the_transfer_commands_name_what_they_act_on() {
        let mut s = logged_in();
        assert_eq!(s.handle("PASV"), FtpAction::Passive(PassiveKind::Classic));
        assert_eq!(s.handle("EPSV"), FtpAction::Passive(PassiveKind::Extended));
        assert_eq!(
            s.handle("RETR /model/a.3mf"),
            FtpAction::Retrieve {
                path: "/model/a.3mf".into()
            }
        );
        assert_eq!(
            s.handle("DELE /cache/a.3mf"),
            FtpAction::Delete {
                path: "/cache/a.3mf".into()
            }
        );
        assert_eq!(
            s.handle("SIZE /cache/a.3mf"),
            FtpAction::Size {
                path: "/cache/a.3mf".into()
            }
        );
    }

    #[test]
    fn list_defaults_to_the_working_directory_and_ignores_ls_flags() {
        let mut s = logged_in();
        s.handle("CWD /cache");
        assert_eq!(
            s.handle("LIST"),
            FtpAction::List {
                path: "/cache".into(),
                names_only: false
            }
        );
        // Some clients send `LIST -la`, which is flags, not a path.
        assert_eq!(
            s.handle("LIST -la"),
            FtpAction::List {
                path: "/cache".into(),
                names_only: false
            }
        );
        assert_eq!(
            s.handle("NLST /model"),
            FtpAction::List {
                path: "/model".into(),
                names_only: true
            }
        );
    }

    #[test]
    fn a_transfer_command_with_no_path_is_a_syntax_error_not_a_transfer() {
        let mut s = logged_in();
        for command in ["STOR", "RETR", "DELE", "SIZE"] {
            assert_eq!(reply(s.handle(command)).code, 501, "{command}");
        }
    }

    // ---- the rest ---------------------------------------------------------

    #[test]
    fn commands_are_case_insensitive() {
        let mut s = logged_in();
        assert_eq!(reply(s.handle("syst")).code, 215);
        assert_eq!(reply(s.handle("TyPe I")).code, 200);
    }

    #[test]
    fn the_relay_answers_feat_the_way_the_printer_does() {
        // docs/protocol.md: the printer replies `502 no-features`, and REST is
        // unsupported. A client that copes with the printer copes with this.
        let mut s = logged_in();
        assert_eq!(reply(s.handle("FEAT")).code, 502);
        assert_eq!(reply(s.handle("REST 100")).code, 502);
    }

    #[test]
    fn active_mode_is_refused_so_the_relay_never_dials_back() {
        let mut s = logged_in();
        assert_eq!(reply(s.handle("PORT 127,0,0,1,4,1")).code, 502);
        assert_eq!(reply(s.handle("EPRT |1|127.0.0.1|1025|")).code, 502);
    }

    #[test]
    fn rename_is_relayed_because_the_printer_supports_it() {
        // Uploading to a temp name and renaming into place is a common FTP
        // idiom, and docs/protocol.md records RNFR/RNTO working on the real
        // printer. Refusing it would break the very flow the relay exists for,
        // on a client the printer itself would have satisfied.
        let mut s = logged_in();
        s.handle("CWD /cache");
        assert_eq!(reply(s.handle("RNFR upload.tmp")).code, 350);
        assert_eq!(
            s.handle("RNTO plate.gcode.3mf"),
            FtpAction::Rename {
                from: "/cache/upload.tmp".into(),
                to: "/cache/plate.gcode.3mf".into(),
            }
        );
        // The pair is consumed, so a second RNTO is out of sequence.
        assert_eq!(reply(s.handle("RNTO /cache/b.3mf")).code, 503);
    }

    #[test]
    fn directories_can_be_made_and_removed() {
        // Also in the printer's capability table; a client that creates a
        // subfolder before uploading should not be stopped by the relay.
        let mut s = logged_in();
        assert_eq!(
            s.handle("MKD /cache/sub"),
            FtpAction::MakeDir {
                path: "/cache/sub".into()
            }
        );
        assert_eq!(
            s.handle("RMD /cache/sub"),
            FtpAction::RemoveDir {
                path: "/cache/sub".into()
            }
        );
        // Still a syntax error with no path.
        assert_eq!(reply(s.handle("MKD")).code, 501);
        assert_eq!(reply(s.handle("RMD")).code, 501);
    }

    #[test]
    fn an_unknown_command_is_answered_not_ignored() {
        // Silence hangs a client that is waiting for a reply line.
        let mut s = logged_in();
        let r = reply(s.handle("FROB /a"));
        assert_eq!(r.code, 502);
        assert!(r.text.contains("FROB"), "{}", r.text);
    }

    #[test]
    fn quit_closes_the_connection() {
        assert_eq!(
            session().handle("QUIT"),
            FtpAction::Close(Reply::new(221, "goodbye"))
        );
    }

    #[test]
    fn implicit_tls_still_answers_the_protection_commands() {
        // Clients send PBSZ/PROT out of habit; refusing looks like a downgrade.
        let mut s = session();
        assert_eq!(reply(s.handle("PBSZ 0")).code, 200);
        assert_eq!(reply(s.handle("PROT P")).code, 200);
        assert_eq!(reply(s.handle("PROT C")).code, 534);
    }

    #[test]
    fn a_reply_is_crlf_terminated() {
        // Bare LF hangs strict clients.
        assert_eq!(Reply::new(220, "hello").to_line(), "220 hello\r\n");
    }
}
