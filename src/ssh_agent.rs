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

use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::Arc;

/// The guest asks the agent which keys it holds.
const REQUEST_IDENTITIES: u8 = 11;
/// The guest asks the agent to sign a blob.
const SIGN_REQUEST: u8 = 13;
/// The agent's "no".
const AGENT_FAILURE: u8 = 5;
/// The agent's list of keys.
const IDENTITIES_ANSWER: u8 = 12;

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

/// An extension request. `ssh` opens every forwarded-agent connection with
/// `session-bind@openssh.com` and carries on when the agent says no.
const EXTENSION: u8 = 27;

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
    /// Refused, and said out loud: the guest asked for something it had no business asking.
    Refuse(&'static str),
    /// Refused in silence: geli does not implement it and the client copes.
    Unsupported,
}

/// Decide a single client request without touching the host's agent.
///
/// Pure, so the policy is testable without a running `ssh-agent`, a VM, or a socket.
pub(crate) fn screen(payload: &[u8], granted: &Granted) -> Screen {
    let Some((&kind, rest)) = payload.split_first() else {
        return Screen::Refuse("empty request");
    };

    match kind {
        REQUEST_IDENTITIES => Screen::Forward,
        SIGN_REQUEST => match ssh_string(rest) {
            Some((blob, _)) if granted.allows(blob) => Screen::Forward,
            Some(_) => Screen::Refuse("sign request for a key this session was not granted"),
            None => Screen::Refuse("malformed sign request"),
        },
        EXTENSION => Screen::Unsupported,
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

/// The relay, as the session sees it.
pub(crate) struct AgentRelay {
    /// Port on the host's loopback. The guest reaches it at slirp's `10.0.2.2:<port>`, the same
    /// alias the egress proxy uses.
    pub(crate) port: u16,
}

/// Listen for the guest, screen every request, and let the host's `ssh-agent` answer the rest.
pub(crate) fn start_agent_relay(auth_sock: &Path, granted: Granted) -> io::Result<AgentRelay> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();
    let auth_sock = Arc::new(auth_sock.to_path_buf());
    let granted = Arc::new(granted);

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let (auth_sock, granted) = (auth_sock.clone(), granted.clone());
            std::thread::spawn(move || {
                if let Err(e) = serve(stream, &auth_sock, &granted) {
                    // One failed connection is not a failed session: ssh retries, and a dead
                    // relay thread must not take the agent down with it.
                    eprintln!("[!] ssh agent relay: {}", e);
                }
            });
        }
    });

    Ok(AgentRelay { port })
}

fn serve(mut guest: TcpStream, auth_sock: &Path, granted: &Granted) -> io::Result<()> {
    let mut agent = UnixStream::connect(auth_sock)?;

    while let Some(request) = read_frame(&mut guest)? {
        match screen(&request, granted) {
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
                write_frame(&mut guest, &filter_identities(&reply, granted))?;
            }
        }
    }
    Ok(())
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
        assert_eq!(screen(&[REQUEST_IDENTITIES], &Granted::All), Screen::Forward);
        assert_eq!(screen(&sign_request(b"key-a"), &Granted::All), Screen::Forward);

        // 17 add, 18 remove, 19 remove-all, 20/21 smartcard, 22 lock, 23 unlock, 25 constrained.
        for mutating in [17u8, 18, 19, 20, 21, 22, 23, 25] {
            assert_eq!(
                screen(&[mutating], &Granted::All),
                Screen::Refuse("agent request other than listing keys or signing"),
                "message type {} was let through",
                mutating
            );
        }

        // An extension is refused too, but silently: ssh negotiates `session-bind@openssh.com` on
        // every forwarded connection and carries on when it is denied. Reporting that trains the
        // user to ignore the line that matters.
        assert_eq!(screen(&[EXTENSION], &Granted::All), Screen::Unsupported);

        assert!(matches!(screen(&[], &Granted::All), Screen::Refuse(_)));
    }

    /// `--ssh-key` names one identity. Without this it would name one and hand over all of them.
    #[test]
    fn granting_one_key_refuses_signatures_for_the_others() {
        let granted = Granted::Only(b"key-a".to_vec());

        assert_eq!(screen(&sign_request(b"key-a"), &granted), Screen::Forward);
        assert_eq!(
            screen(&sign_request(b"key-b"), &granted),
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
        assert_eq!(
            screen(&[SIGN_REQUEST], &Granted::All),
            Screen::Refuse("malformed sign request")
        );
        assert_eq!(
            screen(&[SIGN_REQUEST, 0, 0, 0, 99], &Granted::All),
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
