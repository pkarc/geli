//! The SSH agent relay: the guest signs with the host's key but never reads it.
//!
//! `--ssh` used to copy `~/.ssh/id_*` into the VM, where the agent could read it like any other
//! file and where it stayed valid long after the disposable VM was gone. Here the key never
//! crosses: the guest gets a socket, speaks the agent protocol over it, and the host's `ssh-agent`
//! does the signing.
//!
//! **This is not a relay, it is a filter, and that is the point.** Forwarding an agent wholesale
//! lets whatever is on the other end enumerate every key you have loaded and delete them from your
//! agent. Only `REQUEST_IDENTITIES` and `SIGN_REQUEST` are passed through; everything else is
//! answered with `SSH_AGENT_FAILURE` without the host's agent ever hearing it. When one key was
//! granted with `--ssh-key`, the identities answer is rewritten to hold only that key and sign
//! requests for any other are refused — otherwise `--ssh-key` would name a key and hand over all
//! of them.
//!
//! What this still does not give: for as long as the session runs, the guest can ask for a
//! signature over anything. It cannot steal the key, but it can act as you. `ssh-add -c` is the
//! mitigation — it makes the agent ask before each signature — and `docs/plans/08-reenviar-agente-ssh.md`
//! is plain about the size of the win: permanent exposure becomes exposure bounded by the session.
//!
//! **What a connection can prove about itself.** `ssh` opens every forwarded-agent connection with
//! `session-bind@openssh.com`, which carries the server's host key, the identifier of that
//! connection, and that host key's signature over the identifier. Refusing it threw away the only
//! thing in the protocol that says *who is on the other end*, so it is accepted and checked here
//! instead: the signature is verified, the binding is remembered for that connection, and an
//! authentication signature whose session identifier belongs to no binding is refused. That is not
//! yet a policy — nothing is blocked for *where* it is going — but it is what a policy would have
//! to stand on, and it is what lets a session say at the end where it signed toward.
//! → `docs/plans/09-atar-la-firma-a-su-conexion.md`

use signature::Verifier;
use ssh_encoding::Decode;
use ssh_key::known_hosts::{Entry, HostPatterns};
use ssh_key::public::KeyData;
use ssh_key::{Algorithm, HashAlg, Signature};
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::{Arc, Mutex};

/// The guest asks the agent which keys it holds.
const REQUEST_IDENTITIES: u8 = 11;
/// The guest asks the agent to sign a blob.
const SIGN_REQUEST: u8 = 13;
/// The agent's "no".
const AGENT_FAILURE: u8 = 5;
/// The agent's "yes", with nothing else to say. The answer to an accepted binding.
const AGENT_SUCCESS: u8 = 6;
/// The agent's list of keys.
const IDENTITIES_ANSWER: u8 = 12;
/// A signature, the only reply that means one was actually made.
const SIGN_RESPONSE: u8 = 14;
/// The first byte of what an SSH client signs to authenticate. Everything else a key gets asked to
/// sign — a commit, `ssh-keygen -Y sign` — is not this, and has no connection to be tied to.
const USERAUTH_REQUEST: u8 = 50;

/// A frame larger than this is not an agent message; it is something trying its luck. The largest
/// legitimate request is a sign over a hash with a big RSA key, far under this.
const MAX_FRAME: u32 = 256 * 1024;

/// Which keys the session may use.
///
/// `All` still blocks every mutating request — it means "every key the agent holds", not "do what
/// you like with my agent".
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Granted {
    All,
    Only(Vec<u8>),
}

impl Granted {
    fn allows(&self, blob: &[u8]) -> bool {
        match self {
            Granted::All => true,
            Granted::Only(one) => one == blob,
        }
    }
}

/// An extension request. Every one other than the binding below is still answered with a silent
/// no: `ssh` carries on, and this relay implements none of them.
const EXTENSION: u8 = 27;
/// The extension that names the far end of a connection.
const SESSION_BIND: &[u8] = b"session-bind@openssh.com";

/// One connection's claim about who it is talking to, with the signature to back it.
///
/// **What verifying buys, exactly.** A binding proves whoever sent it holds the private half of
/// `host_key`. It does **not** prove that key belongs to a real server: a guest can generate a
/// host key of its own and sign a binding with it all day. What it cannot do is claim a key it
/// does not hold — and host keys are public, so without the check it could have claimed
/// `github.com` for anything. So a reported destination that `known_hosts` recognises is worth
/// believing, and an unrecognised fingerprint may be one the guest minted.
///
/// The consequence, which is the honest way to state the property: **this report cannot hide a
/// destination, only add invented ones**. A signature is only useful against the server whose key
/// it names, so anything the guest can actually spend is named correctly.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Binding {
    /// The host key of the server at the other end, in wire format.
    pub(crate) host_key: Vec<u8>,
    /// The identifier of this connection. An authentication signature carries it, which is what
    /// ties a signature to a binding.
    session_id: Vec<u8>,
    /// Set when this hop is passing the agent on to a further one. Recorded rather than acted on:
    /// a session that re-forwards is worth being able to say so.
    pub(crate) is_forwarding: bool,
}

/// Every binding one connection has sent. Empty for a client that never sent one.
#[derive(Debug, Default)]
pub(crate) struct Bindings {
    proved: Vec<Binding>,
    /// This connection said who it was talking to and geli could not check the key type. Kept
    /// apart from "said nothing", because the two are different things to read in a report.
    beyond_us: bool,
}

/// A connection binds once. The room above that is for a client doing something reasonable this
/// code has not seen; past it, the guest is growing a list inside geli's own process, and
/// something that is only ever 1 has no business being unbounded.
const MAX_BINDINGS: usize = 16;

impl Bindings {
    fn remember(&mut self, binding: Binding) {
        if !self.proved.contains(&binding) {
            self.proved.push(binding);
        }
    }

    /// Note that this connection bound something geli has no way to check.
    fn remember_uncheckable(&mut self) {
        self.beyond_us = true;
    }

    fn is_full(&self) -> bool {
        self.proved.len() >= MAX_BINDINGS
    }

    fn holding(&self, session_id: &[u8]) -> Option<&Binding> {
        self.proved.iter().find(|b| b.session_id == session_id)
    }

    fn is_empty(&self) -> bool {
        self.proved.is_empty()
    }
}

/// What came of reading a `session-bind@openssh.com` body.
///
/// **Three outcomes, not two, and the third is the one that was missing.** A host key signed by a
/// certificate authority arrives as `ssh-ed25519-cert-v01@openssh.com`, which decodes as
/// `KeyData::Other` and cannot be verified by anything here. Treating that as a failed check
/// printed a loud refusal at a perfectly honest connection — any company that signs its host keys
/// with a CA would see one — and a warning that fires when nothing is wrong hides the one that
/// matters. That lesson is already written down twice (`Screen::Unsupported`, and
/// `docs/why/agents-and-credentials.md`); this is the third place it applies.
enum Bound {
    /// Signed by a key we can check, and it checks out.
    Verified(Binding),
    /// A key type this build cannot verify. Not suspicious, just beyond us: the connection goes
    /// on unbound, silently, exactly like a client that never sent a binding.
    Uncheckable,
    /// Malformed, or a signature that does not match. Nothing honest produces this.
    Bad,
}

/// Read a `session-bind@openssh.com` body and check the host's signature over the session id.
///
/// **The verification is the whole point.** Without it the body is a claim by the guest, and a
/// guest can claim any host key it likes, since host keys are public. What it buys and what it
/// does not is spelled out on [`Binding`].
fn read_binding(body: &[u8]) -> Bound {
    let Some((host_key, rest)) = ssh_string(body) else { return Bound::Bad };
    let Some((session_id, rest)) = ssh_string(rest) else { return Bound::Bad };
    let Some((signature, rest)) = ssh_string(rest) else { return Bound::Bad };
    let Some((&is_forwarding, _)) = rest.split_first() else { return Bound::Bad };

    let Ok(key) = KeyData::decode(&mut &host_key[..]) else { return Bound::Bad };
    let Ok(parsed) = Signature::decode(&mut &signature[..]) else { return Bound::Bad };

    // Asked before verifying, because `verify` cannot tell the two apart: it says no both to a
    // forgery and to a key type it has never heard of.
    if matches!(key.algorithm(), Algorithm::Other(_))
        || matches!(parsed.algorithm(), Algorithm::Other(_))
    {
        return Bound::Uncheckable;
    }

    if key.verify(session_id, &parsed).is_err() {
        return Bound::Bad;
    }

    Bound::Verified(Binding {
        host_key: host_key.to_vec(),
        session_id: session_id.to_vec(),
        is_forwarding: is_forwarding != 0,
    })
}

/// The authentication method every client here actually uses.
///
/// **Measured, not assumed.** The first real session signed and was counted as "not an
/// authentication", because the obvious parse looks for the method `publickey` and OpenSSH has not
/// sent that to an agent in years: since 9.x it asks for `publickey-hostbound-v00@openssh.com`,
/// which puts the server's host key *inside the signed data*. That is a better source of truth
/// than the binding — the signature itself commits to where it is going — so it is read out here
/// and used in preference.
const HOSTBOUND: &[u8] = b"publickey-hostbound-v00@openssh.com";
/// The older method, kept for clients that still send it. No host key inside; it leans on the
/// binding to be named.
const PUBLICKEY: &[u8] = b"publickey";

/// What a sign request is authenticating, when that is what it is doing at all.
#[derive(Debug, PartialEq)]
struct Authentication<'a> {
    /// The connection this authentication belongs to.
    session_id: &'a [u8],
    /// The server the signature itself names, when the method carries one.
    host_key: Option<&'a [u8]>,
}

/// Read an authentication out of what a sign request was asked to sign.
///
/// What a client signs to authenticate is not opaque: the session identifier, then
/// `SSH2_MSG_USERAUTH_REQUEST`, then the user, the service and the method. Anything not shaped
/// like that is some other use of the key — signing a commit, signing a file — and has no
/// connection behind it to check.
fn authentication(data: &[u8]) -> Option<Authentication<'_>> {
    let (session_id, rest) = ssh_string(data)?;
    let (&kind, rest) = rest.split_first()?;
    if kind != USERAUTH_REQUEST {
        return None;
    }
    let (_user, rest) = ssh_string(rest)?;
    let (service, rest) = ssh_string(rest)?;
    if service != b"ssh-connection" {
        return None;
    }

    match ssh_string(rest)? {
        (PUBLICKEY, _) => Some(Authentication { session_id, host_key: None }),
        (HOSTBOUND, rest) => {
            // The method's own trailer: the "has a signature" flag, the algorithm, the key being
            // offered, and then the host key this authentication is for.
            let (_flag, rest) = rest.split_first()?;
            let (_algorithm, rest) = ssh_string(rest)?;
            let (_offered_key, rest) = ssh_string(rest)?;
            let (host_key, _) = ssh_string(rest)?;
            Some(Authentication { session_id, host_key: Some(host_key) })
        }
        _ => None,
    }
}

/// The authentication a sign request carries, straight from the request.
///
/// Both the screening and the accounting need this, and parsing it twice in two places is how the
/// two would come to disagree about what a request was.
fn sign_request_authentication(payload: &[u8]) -> Option<Authentication<'_>> {
    let (&SIGN_REQUEST, rest) = payload.split_first()? else {
        return None;
    };
    let (_key, rest) = ssh_string(rest)?;
    let (data, _) = ssh_string(rest)?;
    authentication(data)
}

/// What the relay does with a request.
///
/// **Refusing and reporting are two different things, and conflating them was measured to be
/// wrong.** The first end-to-end run printed a refusal on every ordinary `ssh` connection,
/// because ssh negotiates an extension it is perfectly happy to be denied. A warning that fires
/// when nothing is wrong is a warning people learn to scroll past — and the one it would hide is
/// the guest trying to reshape the user's agent.
#[derive(Debug, PartialEq)]
pub(crate) enum Screen {
    Forward,
    /// A binding that checked out. Answered by this relay, not by the host's agent, which has no
    /// use for it: the connection it describes is this one.
    Bind(Binding),
    /// A binding naming a key type geli cannot verify — a host key signed by a certificate
    /// authority is the ordinary case. Declined in the same silence as anything unimplemented:
    /// the connection carries on unbound, and nothing is wrong.
    Uncheckable,
    /// Refused, and said out loud: the guest asked for something it had no business asking.
    Refuse(&'static str),
    /// Refused in silence: geli does not implement it and the client copes.
    Unsupported,
}

/// Decide a single client request without touching the host's agent.
///
/// Pure, so the policy is testable without a running `ssh-agent`, a VM, or a socket. The crypto in
/// `read_binding` is part of the decision and stays inside it, for the same reason.
pub(crate) fn screen(payload: &[u8], granted: &Granted, bound: &Bindings) -> Screen {
    let Some((&kind, rest)) = payload.split_first() else {
        return Screen::Refuse("empty request");
    };

    match kind {
        REQUEST_IDENTITIES => Screen::Forward,
        SIGN_REQUEST => match ssh_string(rest) {
            None => Screen::Refuse("malformed sign request"),
            Some((blob, _)) if !granted.allows(blob) => {
                Screen::Refuse("sign request for a key this session was not granted")
            }
            // Not an authentication — a commit, a file — so there is no connection to tie it to,
            // and a client that never bound one gets what it has always got. **Tying every
            // signature would break both**: `--restrict-net` leaves local signing as the only
            // useful thing an agent does there, and a client that is not OpenSSH may send no
            // binding at all. Once a connection *has* bound, it is held to it.
            Some(_) => match sign_request_authentication(payload) {
                None => Screen::Forward,
                Some(auth) => match bound.holding(auth.session_id) {
                    // The signature names one host and the connection proved another. Nothing
                    // honest produces this.
                    Some(binding)
                        if auth.host_key.is_some_and(|key| key != binding.host_key) =>
                    {
                        Screen::Refuse("signature naming a host this connection did not bind")
                    }
                    Some(_) => Screen::Forward,
                    None if bound.is_empty() => Screen::Forward,
                    None => {
                        Screen::Refuse("signature for a connection this session never bound")
                    }
                },
            },
        },
        EXTENSION => match ssh_string(rest) {
            Some((SESSION_BIND, _)) if bound.is_full() => {
                Screen::Refuse("connection binding itself over and over")
            }
            Some((SESSION_BIND, body)) => match read_binding(body) {
                Bound::Verified(binding) => Screen::Bind(binding),
                Bound::Uncheckable => Screen::Uncheckable,
                Bound::Bad => {
                    Screen::Refuse("session binding whose host signature does not check out")
                }
            },
            // Every other extension, silently: ssh negotiates what it is perfectly happy to be
            // denied, and a warning that fires when nothing is wrong is one people learn to skip.
            _ => Screen::Unsupported,
        },
        // Everything that adds, removes or locks. The guest has no business reshaping the user's
        // agent, and these are the requests that would let it.
        _ => Screen::Refuse("agent request other than listing keys or signing"),
    }
}

/// Rewrite an identities answer so it names only the granted key.
///
/// Returned unchanged for anything that is not an identities answer — a sign response has no
/// business being rewritten, and guessing at an unknown reply is how a relay corrupts a protocol
/// it does not fully implement.
pub(crate) fn filter_identities(payload: &[u8], granted: &Granted) -> Vec<u8> {
    if granted == &Granted::All {
        return payload.to_vec();
    }
    let Some((&IDENTITIES_ANSWER, rest)) = payload.split_first() else {
        return payload.to_vec();
    };
    let Some((count, mut rest)) = be_u32(rest) else { return payload.to_vec() };

    let mut kept: Vec<(&[u8], &[u8])> = Vec::new();
    for _ in 0..count {
        let Some((blob, after_blob)) = ssh_string(rest) else { return payload.to_vec() };
        let Some((comment, after_comment)) = ssh_string(after_blob) else {
            return payload.to_vec();
        };
        if granted.allows(blob) {
            kept.push((blob, comment));
        }
        rest = after_comment;
    }

    let mut out = vec![IDENTITIES_ANSWER];
    out.extend_from_slice(&(kept.len() as u32).to_be_bytes());
    for (blob, comment) in kept {
        out.extend_from_slice(&(blob.len() as u32).to_be_bytes());
        out.extend_from_slice(blob);
        out.extend_from_slice(&(comment.len() as u32).to_be_bytes());
        out.extend_from_slice(comment);
    }
    out
}

fn be_u32(buf: &[u8]) -> Option<(u32, &[u8])> {
    let (head, rest) = buf.split_at_checked(4)?;
    Some((u32::from_be_bytes(head.try_into().ok()?), rest))
}

/// An SSH wire string: a big-endian length and that many bytes.
fn ssh_string(buf: &[u8]) -> Option<(&[u8], &[u8])> {
    let (len, rest) = be_u32(buf)?;
    rest.split_at_checked(len as usize)
}

/// The key blob out of an OpenSSH `.pub` line — `ssh-ed25519 AAAA… comment`.
///
/// It is the same blob the agent reports for that key, which is what makes `--ssh-key` able to
/// name one identity out of several without asking the agent first.
pub(crate) fn public_key_blob(line: &str) -> Option<Vec<u8>> {
    base64_decode(line.split_whitespace().nth(1)?)
}

/// Enough base64 to read a `.pub` file. A dependency for twenty lines of table lookup is a
/// dependency to keep updated forever.
fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let value = |c: u8| -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => (c - b'A') as u32,
            b'a'..=b'z' => (c - b'a') as u32 + 26,
            b'0'..=b'9' => (c - b'0') as u32 + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        })
    };

    let body: Vec<u8> = text.bytes().filter(|c| *c != b'=').collect();
    let mut out = Vec::with_capacity(body.len() * 3 / 4);
    let mut acc: u32 = 0;
    let mut bits = 0;
    for c in body {
        acc = (acc << 6) | value(c)?;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

/// A length-prefixed agent frame, or `None` at a clean end of stream.
fn read_frame(stream: &mut impl Read) -> io::Result<Option<Vec<u8>>> {
    let mut len = [0u8; 4];
    match stream.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_be_bytes(len);
    if len == 0 || len > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "agent frame out of range"));
    }
    let mut payload = vec![0u8; len as usize];
    stream.read_exact(&mut payload)?;
    Ok(Some(payload))
}

fn write_frame(stream: &mut impl Write, payload: &[u8]) -> io::Result<()> {
    stream.write_all(&(payload.len() as u32).to_be_bytes())?;
    stream.write_all(payload)?;
    stream.flush()
}

/// What the session did with the key, for the line that reports it afterwards.
///
/// Counted rather than logged as it happens: the terminal belongs to the agent while the session
/// runs, so this is read once at the end, next to the egress figures.
#[derive(Debug, Default)]
pub(crate) struct Signings {
    /// Host keys signed toward, with how many signatures each. Wire format, resolved to a name
    /// only when it is printed.
    pub(crate) toward: Vec<(Vec<u8>, usize)>,
    /// Authentications by a client that bound no connection, so there is nothing to name.
    pub(crate) unbound: usize,
    /// Authentications on a connection that *did* say where it was going, in a key type geli
    /// cannot verify. Counted apart from `unbound`: one is the client saying nothing, the other
    /// is geli not understanding what it said, and only the second is geli's shortcoming.
    pub(crate) unchecked: usize,
    /// Signatures that are not an authentication: commits, files. No destination by nature.
    pub(crate) local: usize,
}

impl Signings {
    fn record(&mut self, host_key: &[u8]) {
        match self.toward.iter_mut().find(|(k, _)| k == host_key) {
            Some((_, count)) => *count += 1,
            None => self.toward.push((host_key.to_vec(), 1)),
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.toward.is_empty() && self.unbound == 0 && self.unchecked == 0 && self.local == 0
    }
}

/// The relay, as the session sees it.
pub(crate) struct AgentRelay {
    /// Port on the host's loopback. The guest reaches it at slirp's `10.0.2.2:<port>`, the same
    /// alias the egress proxy uses.
    pub(crate) port: u16,
    /// Shared with every connection thread, like the proxy's own stats.
    pub(crate) signings: Arc<Mutex<Signings>>,
}

/// Listen for the guest, screen every request, and let the host's `ssh-agent` answer the rest.
pub(crate) fn start_agent_relay(auth_sock: &Path, granted: Granted) -> io::Result<AgentRelay> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();
    let auth_sock = Arc::new(auth_sock.to_path_buf());
    let granted = Arc::new(granted);
    let signings = Arc::new(Mutex::new(Signings::default()));
    let shared = signings.clone();

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let (auth_sock, granted) = (auth_sock.clone(), granted.clone());
            let signings = shared.clone();
            std::thread::spawn(move || {
                if let Err(e) = serve(stream, &auth_sock, &granted, &signings) {
                    // One failed connection is not a failed session: ssh retries, and a dead
                    // relay thread must not take the agent down with it.
                    eprintln!("[!] ssh agent relay: {}", e);
                }
            });
        }
    });

    Ok(AgentRelay { port, signings })
}

fn serve(
    mut guest: TcpStream,
    auth_sock: &Path,
    granted: &Granted,
    signings: &Mutex<Signings>,
) -> io::Result<()> {
    let mut agent = UnixStream::connect(auth_sock)?;
    // Per connection, not per session: a binding describes the one connection that sent it, and
    // sharing them across connections would let one borrow another's destination.
    let mut bound = Bindings::default();

    while let Some(request) = read_frame(&mut guest)? {
        match screen(&request, granted, &bound) {
            Screen::Bind(binding) => {
                bound.remember(binding);
                // Answered here. The host's agent has no use for a binding about a connection it
                // is not on the other end of.
                write_frame(&mut guest, &[AGENT_SUCCESS])?;
            }
            Screen::Uncheckable => {
                bound.remember_uncheckable();
                write_frame(&mut guest, &[AGENT_FAILURE])?;
            }
            Screen::Unsupported => write_frame(&mut guest, &[AGENT_FAILURE])?,
            Screen::Refuse(reason) => {
                eprintln!("[!] ssh agent relay refused a {}", reason);
                write_frame(&mut guest, &[AGENT_FAILURE])?;
            }
            Screen::Forward => {
                write_frame(&mut agent, &request)?;
                let Some(reply) = read_frame(&mut agent)? else {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "the host's ssh-agent closed mid-request",
                    ));
                };

                if let Ok(mut signings) = signings.lock() {
                    account(&request, &reply, &bound, &mut signings);
                }

                write_frame(&mut guest, &filter_identities(&reply, granted))?;
            }
        }
    }
    Ok(())
}

/// Count what the host's agent just did, from the request and the answer it got.
///
/// **The answer is half of it.** Letting a request through is not the same as the agent granting
/// it — a locked agent, or one that no longer holds the key, replies with a failure — so only a
/// reply that is a signature counts as one. Separated from the socket work so this is testable
/// without an agent to refuse on cue.
fn account(request: &[u8], reply: &[u8], bound: &Bindings, signings: &mut Signings) {
    if reply.first() != Some(&SIGN_RESPONSE) {
        return;
    }
    match sign_request_authentication(request) {
        None => signings.local += 1,
        // The host key the signature itself names comes first: it is the one the server being
        // talked to has to accept, so it is the one a guest gains nothing by lying about.
        Some(auth) => {
            let proved = bound.holding(auth.session_id).map(|b| &b.host_key[..]);
            match auth.host_key.or(proved) {
                Some(key) => signings.record(key),
                None if bound.beyond_us => signings.unchecked += 1,
                None => signings.unbound += 1,
            }
        }
    }
}

/// The one line a session says about what it signed.
///
/// Pure: the caller reads `known_hosts`, this decides the wording. Every part of it is a count of
/// something that happened, because a line that rounds down is a line that lies by omission.
pub(crate) fn render_signings(signings: &Signings, known: &[Entry]) -> String {
    let mut parts: Vec<String> = signings
        .toward
        .iter()
        .map(|(key, count)| format!("{} ({})", host_label(key, known), count))
        .collect();
    if signings.unbound > 0 {
        parts.push(format!("{} unnamed, the client bound no connection", signings.unbound));
    }
    if signings.unchecked > 0 {
        parts.push(format!(
            "{} unnamed, geli cannot verify that host key type",
            signings.unchecked
        ));
    }
    if signings.local > 0 {
        parts.push(format!("{} not an authentication", signings.local));
    }
    format!("signed {}", parts.join(", "))
}

/// What to call a host key: the name `known_hosts` has for it, or its fingerprint.
///
/// **A `known_hosts` written with `HashKnownHosts yes` cannot be read backwards** — the names in it
/// are hashes, and there is no host key to start from. The fingerprint is then the honest answer,
/// not a degraded one: it is what the user's own file is able to say.
pub(crate) fn host_label(host_key: &[u8], known: &[Entry]) -> String {
    let Ok(key) = KeyData::decode(&mut &host_key[..]) else {
        return "an unreadable host key".to_string();
    };

    for entry in known {
        if entry.public_key().key_data() != &key {
            continue;
        }
        if let HostPatterns::Patterns(names) = entry.host_patterns() {
            if let Some(first) = names.first() {
                // A key the user marked `@revoked` is still the name of a host, and a session
                // signing toward it is the most interesting thing this line could ever say. It
                // would be absurd to print the name and drop the one word that matters.
                return match entry.marker() {
                    Some(mark) => format!("{} (marked {} in known_hosts)", first, mark.as_str()),
                    None => first.clone(),
                };
            }
        }
    }
    key.fingerprint(HashAlg::Sha256).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn string(bytes: &[u8]) -> Vec<u8> {
        let mut out = (bytes.len() as u32).to_be_bytes().to_vec();
        out.extend_from_slice(bytes);
        out
    }

    fn sign_request(blob: &[u8]) -> Vec<u8> {
        let mut out = vec![SIGN_REQUEST];
        out.extend(string(blob));
        out.extend(string(b"data to sign"));
        out.extend(0u32.to_be_bytes());
        out
    }

    /// A throwaway ed25519 key, standing in for a server's host key. Generated for these tests and
    /// never anywhere else — it signs the bindings below so the verification has something real to
    /// check, which a hand-written byte string could not give.
    const TEST_HOST_KEY: &str = "\
-----BEGIN OPENSSH PRIVATE KEY-----
b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW
QyNTUxOQAAACBCc3vy2A/Z9nqtjYYuovXy9HW4FKCURHcZ766k06G38AAAAJi5VlrZuVZa
2QAAAAtzc2gtZWQyNTUxOQAAACBCc3vy2A/Z9nqtjYYuovXy9HW4FKCURHcZ766k06G38A
AAAEA3S76Er0+hVOjZn4LtYR6c2nT+YkT8ctfbGPr6NwkHb0Jze/LYD9n2eq2Nhi6i9fL0
dbgUoJREdxnvrqTTobfwAAAAEmdlbGkgdGVzdCBob3N0IGtleQECAw==
-----END OPENSSH PRIVATE KEY-----
";

    fn wire(value: &impl ssh_encoding::Encode) -> Vec<u8> {
        let mut out = Vec::new();
        value.encode(&mut out).expect("encodes");
        out
    }

    /// A binding that claims a host key other than the one doing the signing — someone else's.
    fn session_bind_claiming(session_id: &[u8], claimed: &[u8]) -> Vec<u8> {
        let honest = session_bind(session_id, None);
        let real = string(&test_host_key_blob());
        let at = honest.windows(real.len()).position(|w| w == real).expect("the key is in there");
        let mut out = honest[..at].to_vec();
        out.extend(string(claimed));
        out.extend_from_slice(&honest[at + real.len()..]);
        out
    }

    /// A host key in the shape a certificate authority signs: the algorithm name says `-cert-`,
    /// and no amount of crypto features makes that verifiable here.
    fn certificate_host_key() -> Vec<u8> {
        let name = b"ssh-ed25519-cert-v01@openssh.com";
        let mut blob = string(name);
        blob.extend(string(&[7u8; 32]));
        blob
    }

    /// A key that is structurally fine and belongs to nobody here: the test key with its material
    /// altered. Standing in for a host key the guest claims but cannot sign for.
    fn someone_elses_host_key() -> Vec<u8> {
        let mut blob = test_host_key_blob();
        *blob.last_mut().unwrap() ^= 0xff;
        blob
    }

    /// A `session-bind@openssh.com` request, signed by the test host key unless told to lie.
    fn session_bind(session_id: &[u8], signed_instead: Option<&[u8]>) -> Vec<u8> {
        let host = ssh_key::PrivateKey::from_openssh(TEST_HOST_KEY).expect("a key");
        // Spelled out rather than `host.sign(..)`: the inherent `sign` makes an `SshSig`, which is
        // a different thing — the armoured format for signing files, not a raw SSH signature.
        let message = signed_instead.unwrap_or(session_id);
        let signature: Signature = signature::Signer::sign(&host, message);

        let mut body = Vec::new();
        body.extend(string(&wire(host.public_key().key_data())));
        body.extend(string(session_id));
        body.extend(string(&wire(&signature)));
        body.push(0);

        let mut out = vec![EXTENSION];
        out.extend(string(SESSION_BIND));
        out.extend(body);
        out
    }

    fn test_host_key_blob() -> Vec<u8> {
        let host = ssh_key::PrivateKey::from_openssh(TEST_HOST_KEY).expect("a key");
        wire(host.public_key().key_data())
    }

    /// What an SSH client actually signs to authenticate: the session identifier first, then the
    /// userauth request. This shape is the whole reason a signature can be tied to a connection.
    ///
    /// `toward` makes it the hostbound method every current OpenSSH uses, which carries the
    /// server's host key inside the signed data. Captured from a real session, not guessed —
    /// see the note on `HOSTBOUND`.
    fn signed_authentication(session_id: &[u8], toward: Option<&[u8]>) -> Vec<u8> {
        let mut data = string(session_id);
        data.push(USERAUTH_REQUEST);
        data.extend(string(b"git"));
        data.extend(string(b"ssh-connection"));
        match toward {
            None => data.extend(string(PUBLICKEY)),
            Some(host_key) => {
                data.extend(string(HOSTBOUND));
                data.push(1);
                data.extend(string(b"ssh-ed25519"));
                data.extend(string(b"the key being offered"));
                data.extend(string(host_key));
            }
        }
        data
    }

    fn sign_request_over(blob: &[u8], data: &[u8]) -> Vec<u8> {
        let mut out = vec![SIGN_REQUEST];
        out.extend(string(blob));
        out.extend(string(data));
        out.extend(0u32.to_be_bytes());
        out
    }

    fn identities_answer(keys: &[(&[u8], &[u8])]) -> Vec<u8> {
        let mut out = vec![IDENTITIES_ANSWER];
        out.extend_from_slice(&(keys.len() as u32).to_be_bytes());
        for (blob, comment) in keys {
            out.extend(string(blob));
            out.extend(string(comment));
        }
        out
    }

    /// The whole reason this is a filter and not a pipe. A forwarded agent lets the far end add
    /// and remove keys; these are the requests that would do it.
    #[test]
    fn only_listing_and_signing_reach_the_host_agent() {
        let loose = Bindings::default();
        assert_eq!(screen(&[REQUEST_IDENTITIES], &Granted::All, &loose), Screen::Forward);
        assert_eq!(screen(&sign_request(b"key-a"), &Granted::All, &loose), Screen::Forward);

        // 17 add, 18 remove, 19 remove-all, 20/21 smartcard, 22 lock, 23 unlock, 25 constrained.
        for mutating in [17u8, 18, 19, 20, 21, 22, 23, 25] {
            assert_eq!(
                screen(&[mutating], &Granted::All, &loose),
                Screen::Refuse("agent request other than listing keys or signing"),
                "message type {} was let through",
                mutating
            );
        }

        // An extension this relay does not implement is refused too, but silently: ssh negotiates
        // what it is perfectly happy to be denied, and reporting that trains the user to ignore
        // the line that matters. `session-bind@openssh.com` is the one exception, below.
        assert_eq!(screen(&[EXTENSION], &Granted::All, &loose), Screen::Unsupported);

        assert!(matches!(screen(&[], &Granted::All, &loose), Screen::Refuse(_)));
    }

    /// `--ssh-key` names one identity. Without this it would name one and hand over all of them.
    #[test]
    fn granting_one_key_refuses_signatures_for_the_others() {
        let granted = Granted::Only(b"key-a".to_vec());
        let loose = Bindings::default();

        assert_eq!(screen(&sign_request(b"key-a"), &granted, &loose), Screen::Forward);
        assert_eq!(
            screen(&sign_request(b"key-b"), &granted, &loose),
            Screen::Refuse("sign request for a key this session was not granted")
        );

        // The listing is rewritten rather than refused: ssh enumerates before it signs, and a
        // refusal there would simply stop it finding the key it is allowed to use.
        let answer = identities_answer(&[(b"key-a", b"mine"), (b"key-b", b"someone else's")]);
        assert_eq!(
            filter_identities(&answer, &granted),
            identities_answer(&[(b"key-a", b"mine")])
        );
    }

    /// A truncated frame must refuse, not index past the end.
    #[test]
    fn a_malformed_request_is_refused_rather_than_trusted() {
        let loose = Bindings::default();
        assert_eq!(
            screen(&[SIGN_REQUEST], &Granted::All, &loose),
            Screen::Refuse("malformed sign request")
        );
        assert_eq!(
            screen(&[SIGN_REQUEST, 0, 0, 0, 99], &Granted::All, &loose),
            Screen::Refuse("malformed sign request")
        );

        // A reply this code does not understand is passed through untouched rather than guessed at.
        let granted = Granted::Only(b"key-a".to_vec());
        for odd in [vec![], vec![14u8, 1, 2, 3], vec![IDENTITIES_ANSWER, 0, 0]] {
            assert_eq!(filter_identities(&odd, &granted), odd);
        }
    }

    /// `Granted::All` still means "every key the agent holds", never "reshape my agent" — the
    /// listing passes through whole.
    #[test]
    fn granting_everything_leaves_the_listing_alone() {
        let answer = identities_answer(&[(b"key-a", b"one"), (b"key-b", b"two")]);
        assert_eq!(filter_identities(&answer, &Granted::All), answer);
    }

    /// **The test that earns the crypto dependency.** Without verifying the host's signature, a
    /// binding is the guest's word for who it is talking to, and a guest that can claim a host key
    /// — they are public — can put any name it likes in front of the user.
    #[test]
    fn a_binding_is_believed_only_when_the_host_really_signed_it() {
        let loose = Bindings::default();

        let honest = session_bind(b"session-one", None);
        let Screen::Bind(binding) = screen(&honest, &Granted::All, &loose) else {
            panic!("a correctly signed binding was not believed");
        };
        assert_eq!(binding.host_key, test_host_key_blob());
        assert!(!binding.is_forwarding);

        // The same host key and a signature over something else: the shape of an attacker replaying
        // a signature it has, against a session identifier it wants.
        let borrowed = session_bind(b"session-one", Some(b"some other session"));
        assert_eq!(
            screen(&borrowed, &Granted::All, &loose),
            Screen::Refuse("session binding whose host signature does not check out")
        );

        let mut truncated = session_bind(b"session-one", None);
        truncated.truncate(truncated.len() - 20);
        assert_eq!(
            screen(&truncated, &Granted::All, &loose),
            Screen::Refuse("session binding whose host signature does not check out")
        );
    }

    /// Once a connection has said which connection it is, it is held to it. This is what stops a
    /// signature obtained for one destination being presented as belonging to another.
    #[test]
    fn a_bound_connection_only_signs_for_what_it_bound() {
        let mut bound = Bindings::default();
        let request = session_bind(b"session-one", None);
        let Screen::Bind(binding) = screen(&request, &Granted::All, &bound) else {
            panic!("the binding should have been believed");
        };
        bound.remember(binding);
        let host = test_host_key_blob();

        let mine = sign_request_over(b"key-a", &signed_authentication(b"session-one", Some(&host)));
        assert_eq!(screen(&mine, &Granted::All, &bound), Screen::Forward);

        let old_style = sign_request_over(b"key-a", &signed_authentication(b"session-one", None));
        assert_eq!(screen(&old_style, &Granted::All, &bound), Screen::Forward);

        let elsewhere = sign_request_over(b"key-a", &signed_authentication(b"session-two", None));
        assert_eq!(
            screen(&elsewhere, &Granted::All, &bound),
            Screen::Refuse("signature for a connection this session never bound")
        );

        // The signature names one server and the connection proved another. The hostbound method
        // is what makes this visible at all: the destination is inside what is being signed.
        let elsewhere = someone_elses_host_key();
        let lying =
            sign_request_over(b"key-a", &signed_authentication(b"session-one", Some(&elsewhere)));
        assert_eq!(
            screen(&lying, &Granted::All, &bound),
            Screen::Refuse("signature naming a host this connection did not bind")
        );

        // A signature that is not an authentication has no connection to belong to — signing a
        // commit, signing a file. It is also the only useful thing an agent does under
        // `--restrict-net`, so tying it to a binding would break that combination outright.
        let commit = sign_request_over(b"key-a", b"not a userauth request at all");
        assert_eq!(screen(&commit, &Granted::All, &bound), Screen::Forward);
    }

    /// A client that never binds gets what it always got. OpenSSH always binds; others may not,
    /// and this commit changes no policy — it only makes the destination knowable.
    #[test]
    fn a_connection_that_bound_nothing_signs_as_before() {
        let loose = Bindings::default();
        let data = signed_authentication(b"a session nobody bound", None);
        let request = sign_request_over(b"key-a", &data);
        assert_eq!(screen(&request, &Granted::All, &loose), Screen::Forward);
    }

    /// The method OpenSSH really uses. The first real session was counted as "not an
    /// authentication" because this is not `publickey`, and the destination it carries is the
    /// strongest thing here: the server has to accept what was signed, so the guest cannot pick
    /// it freely.
    #[test]
    fn the_hostbound_method_names_its_destination_inside_the_signature() {
        let host = test_host_key_blob();
        let data = signed_authentication(b"session-one", Some(&host));
        assert_eq!(
            authentication(&data),
            Some(Authentication { session_id: b"session-one", host_key: Some(&host) })
        );

        let old_style = signed_authentication(b"session-one", None);
        assert_eq!(
            authentication(&old_style),
            Some(Authentication { session_id: b"session-one", host_key: None })
        );

        assert_eq!(authentication(b"a commit, or a file"), None);
        // A userauth request for something that is not this service is not ours to interpret.
        let mut other = string(b"session-one");
        other.push(USERAUTH_REQUEST);
        other.extend(string(b"git"));
        other.extend(string(b"some-other-service"));
        other.extend(string(PUBLICKEY));
        assert_eq!(authentication(&other), None);
    }

    /// **The limit of what verifying a binding buys, pinned so nobody reads more into it.** A
    /// guest can make a host key of its own and sign a perfectly valid binding with it. What it
    /// cannot do is claim a key it does not hold — and host keys are public, so without the check
    /// it could have claimed github.com's for anything. The report can gain invented destinations;
    /// it cannot lose a real one, because a signature only spends against the server it names.
    #[test]
    fn a_guest_can_bind_a_key_of_its_own_but_not_one_it_does_not_hold() {
        let loose = Bindings::default();

        // Its own key, honestly signed: believed, and named as whatever it is — which
        // `known_hosts` will not recognise.
        let own = session_bind(b"session-one", None);
        assert!(matches!(screen(&own, &Granted::All, &loose), Screen::Bind(_)));
        assert!(host_label(&test_host_key_blob(), &[]).starts_with("SHA256:"));

        // Someone else's key, which it cannot sign for. This is the case the verification exists
        // for: a public key is public, and claiming one is free.
        let claimed = session_bind_claiming(b"session-one", &someone_elses_host_key());
        assert_eq!(
            screen(&claimed, &Granted::All, &loose),
            Screen::Refuse("session binding whose host signature does not check out")
        );
    }

    /// **A host key signed by a CA is the ordinary case, not an attack.** It arrives as
    /// `ssh-ed25519-cert-v01@openssh.com`, decodes as `KeyData::Other`, and nothing here can
    /// verify it. Calling that a failed check printed a loud refusal at every honest connection
    /// to a company that signs its host keys — the exact failure the silent branch exists for.
    #[test]
    fn a_host_key_geli_cannot_check_is_declined_in_silence() {
        let loose = Bindings::default();
        let certificate = session_bind_claiming(b"session-one", &certificate_host_key());

        assert_eq!(screen(&certificate, &Granted::All, &loose), Screen::Uncheckable);

        // And the connection carries on exactly like one that never bound: the signature is
        // forwarded, and the report says why it has no name rather than inventing one.
        let mut bound = Bindings::default();
        bound.remember_uncheckable();
        let request = sign_request_over(b"key-a", &signed_authentication(b"session-one", None));
        assert_eq!(screen(&request, &Granted::All, &bound), Screen::Forward);

        let mut signings = Signings::default();
        account(&request, &[SIGN_RESPONSE, 0], &bound, &mut signings);
        assert_eq!((signings.unchecked, signings.unbound), (1, 0));
        assert!(render_signings(&signings, &[]).contains("cannot verify that host key type"));

        // A client that said nothing is still counted apart: that one is not geli's shortcoming.
        let mut quiet = Signings::default();
        account(&request, &[SIGN_RESPONSE, 0], &Bindings::default(), &mut quiet);
        assert_eq!((quiet.unchecked, quiet.unbound), (0, 1));
    }

    /// A connection binds once. Growing a list inside geli's process is not something a guest gets
    /// to do without limit.
    #[test]
    fn a_connection_cannot_bind_itself_without_end() {
        let mut bound = Bindings::default();
        for n in 0..MAX_BINDINGS {
            let request = session_bind(format!("session-{}", n).as_bytes(), None);
            let Screen::Bind(binding) = screen(&request, &Granted::All, &bound) else {
                panic!("binding {} should have been believed", n);
            };
            bound.remember(binding);
        }

        let one_too_many = session_bind(b"session-last", None);
        assert_eq!(
            screen(&one_too_many, &Granted::All, &bound),
            Screen::Refuse("connection binding itself over and over")
        );
    }

    /// A signature the host's agent refused is not a signature. Counting on the way out rather
    /// than on the way in is the difference between reporting what happened and what was asked.
    #[test]
    fn only_signatures_the_agent_actually_made_are_counted() {
        let bound = Bindings::default();
        let host = test_host_key_blob();
        let request = sign_request_over(b"key-a", &signed_authentication(b"s", Some(&host)));

        let mut refused = Signings::default();
        account(&request, &[AGENT_FAILURE], &bound, &mut refused);
        assert!(refused.is_empty(), "a refusal was counted as a signature");

        let mut granted = Signings::default();
        account(&request, &[SIGN_RESPONSE, 0, 0, 0, 1, 7], &bound, &mut granted);
        assert_eq!(granted.toward, vec![(host, 1)]);

        // A listing is not a signature either.
        let mut listed = Signings::default();
        account(&[REQUEST_IDENTITIES], &[IDENTITIES_ANSWER, 0, 0, 0, 0], &bound, &mut listed);
        assert!(listed.is_empty());
    }

    /// The line a session ends with. A host key becomes a name only if `known_hosts` can say one;
    /// hashed, it cannot be read backwards, and the fingerprint is the honest answer.
    #[test]
    fn the_closing_line_names_what_known_hosts_can_name() {
        let mut signings = Signings::default();
        signings.record(&test_host_key_blob());
        signings.record(&test_host_key_blob());
        signings.unbound = 1;
        signings.local = 3;

        let host = ssh_key::PrivateKey::from_openssh(TEST_HOST_KEY).unwrap();
        let line = format!("git.example.com {}", host.public_key().to_openssh().unwrap());
        let known: Vec<Entry> = ssh_key::KnownHosts::new(&line).collect::<Result<_, _>>().unwrap();

        assert_eq!(
            render_signings(&signings, &known),
            "signed git.example.com (2), 1 unnamed, the client bound no connection, \
             3 not an authentication"
        );

        // Nothing to match it against: the fingerprint, not a guess and not silence.
        let unknown = render_signings(&signings, &[]);
        assert!(unknown.contains("SHA256:"), "expected a fingerprint, got {}", unknown);

        // A key the user revoked, signed toward. Printing the name and swallowing the marker
        // would make the most alarming line this can print look like the most ordinary one.
        let revoked = format!("@revoked {}", line);
        let marked: Vec<Entry> = ssh_key::KnownHosts::new(&revoked).flatten().collect();
        assert!(
            render_signings(&signings, &marked)
                .contains("git.example.com (marked @revoked in known_hosts) (2)"),
            "the revocation went unsaid: {}",
            render_signings(&signings, &marked)
        );
    }

    /// Against the host's real `ssh-agent`, which CI does not have — so it is `#[ignore]`d and
    /// run by hand:
    ///
    /// ```text
    /// cargo test the_relay_answers_a_real_agent -- --ignored --nocapture
    /// ```
    ///
    /// The unit tests above decide the policy; this one is the only thing that shows the framing
    /// and the Unix socket are right, which no amount of pure testing can.
    #[test]
    #[ignore]
    fn the_relay_answers_a_real_agent() {
        use std::net::TcpStream;

        let sock = std::env::var("SSH_AUTH_SOCK").expect("no agent: run ssh-add first");
        let relay = start_agent_relay(Path::new(&sock), Granted::All).unwrap();

        let mut client = TcpStream::connect(("127.0.0.1", relay.port)).unwrap();
        write_frame(&mut client, &[REQUEST_IDENTITIES]).unwrap();
        let reply = read_frame(&mut client).unwrap().expect("no reply");
        assert_eq!(reply[0], IDENTITIES_ANSWER);
        let count = u32::from_be_bytes(reply[1..5].try_into().unwrap());
        println!("the host agent holds {} key(s)", count);
        assert!(count > 0, "load a key with ssh-add before running this");

        // And the thing the filter exists for: removing every key must not reach the agent.
        write_frame(&mut client, &[19]).unwrap();
        assert_eq!(read_frame(&mut client).unwrap().unwrap(), vec![AGENT_FAILURE]);

        // The agent still holds them, which is the assertion that matters.
        write_frame(&mut client, &[REQUEST_IDENTITIES]).unwrap();
        let after = read_frame(&mut client).unwrap().unwrap();
        assert_eq!(u32::from_be_bytes(after[1..5].try_into().unwrap()), count);
    }

    /// The blob in a `.pub` file is the one the agent reports, which is what lets `--ssh-key`
    /// pick one identity out of several without asking the agent first.
    #[test]
    fn a_public_key_line_yields_the_blob_the_agent_reports() {
        // A real ed25519 line: the blob begins with the string "ssh-ed25519".
        let line = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIEx2Q1NgYsJ0ZBKRPqz6s9T1yqkuP1Qe1q7vB4oZqR3s me@host";
        let blob = public_key_blob(line).expect("expected a blob");
        assert_eq!(&blob[4..15], b"ssh-ed25519");
        assert_eq!(u32::from_be_bytes(blob[..4].try_into().unwrap()), 11);

        assert_eq!(public_key_blob("ssh-ed25519"), None, "no second field");
        assert_eq!(public_key_blob("ssh-ed25519 not!base64 c"), None);
    }
}
