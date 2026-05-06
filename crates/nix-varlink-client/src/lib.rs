//! Client for the Nix derivation-builder Varlink IPC.
//!
//! Implements the three methods of the `org.nix.derivation-builder` interface
//! defined by upstream PR <https://github.com/NixOS/nix/pull/13768>:
//!
//! * [`VarlinkClient::add_to_store_nar`] — add a file/directory to the store
//!   by streaming a NAR over an `SCM_RIGHTS`-passed file descriptor.
//! * [`VarlinkClient::add_derivation`] — register a new derivation with the
//!   store.
//! * [`VarlinkClient::submit_output`] — declare a path as a named output of
//!   the calling derivation.
//!
//! The wire format is null-terminated JSON over a Unix stream socket. The
//! socket path is read from `$NIX_VARLINK_REMOTE`.

use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::Path;

use harmonia_store_core::derivation::Derivation;
use harmonia_store_core::store_path::{StoreDir, StorePath};
use nix::sys::socket::{
    sendmsg, socketpair, AddressFamily, ControlMessage, MsgFlags, SockFlag, SockType,
};
use serde_json::{json, Value};

/// Environment variable that holds the Varlink socket path inside a build
/// sandbox. Set by `nix` when the derivation has the `builder-rpc-v1`
/// system feature.
pub const SOCKET_ENV: &str = "NIX_VARLINK_REMOTE";

const INTERFACE: &str = "org.nix.derivation-builder";

/// Hard cap on a single Varlink reply. Replies are tiny JSON envelopes; a
/// runaway server (or a torn connection that never sends `\0`) must not be
/// allowed to exhaust memory while we read byte-by-byte for the terminator.
const MAX_REPLY_BYTES: usize = 1 << 20;

#[derive(thiserror::Error, Debug)]
pub enum VarlinkError {
    #[error("server returned NoFileDescriptor")]
    NoFileDescriptor,
    #[error("server returned InvalidNar")]
    InvalidNar,
    #[error("server returned InvalidPath")]
    InvalidPath,
    #[error("server returned DuplicateOutput")]
    DuplicateOutput,
    #[error("server returned unknown error: {0}")]
    Other(String),
    #[error("transport error: {0}")]
    Transport(#[from] io::Error),
    #[error("nix syscall error: {0}")]
    Nix(#[from] nix::errno::Errno),
    #[error("malformed reply: {0}")]
    Protocol(String),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, VarlinkError>;

pub struct VarlinkClient {
    socket: UnixStream,
    /// Reused read buffer for null-terminated JSON replies.
    buf: Vec<u8>,
}

impl VarlinkClient {
    /// Connect using `$NIX_VARLINK_REMOTE`. Returns `Ok(None)` when the env
    /// var is unset — caller should fall back to the legacy subprocess path.
    pub fn connect_from_env() -> Result<Option<Self>> {
        match std::env::var(SOCKET_ENV) {
            Ok(p) => Ok(Some(Self::connect(Path::new(&p))?)),
            Err(_) => Ok(None),
        }
    }

    pub fn connect(path: &Path) -> Result<Self> {
        let socket = UnixStream::connect(path)?;
        Ok(Self {
            socket,
            buf: Vec::with_capacity(4096),
        })
    }

    /// `AddToStore(name, "nar")`. The closure receives the writer end of an
    /// internal `socketpair` as a `UnixStream`; whatever it writes is
    /// interpreted as a NAR by the server. The reader end is passed to the
    /// server via `SCM_RIGHTS`.
    pub fn add_to_store_nar<F>(
        &mut self,
        store_dir: &StoreDir,
        name: &str,
        write_nar: F,
    ) -> Result<StorePath>
    where
        F: FnOnce(&mut UnixStream) -> io::Result<()>,
    {
        let (reader, writer) = socketpair(
            AddressFamily::Unix,
            SockType::Stream,
            None,
            SockFlag::empty(),
        )?;
        let reader: OwnedFd = reader;
        let writer: OwnedFd = writer;

        let request = json!({
            "method": format!("{INTERFACE}.AddToStore"),
            "parameters": {
                "name": name,
                "method": "nar",
                "descriptor": 0,
            },
        });
        let mut framed = serde_json::to_vec(&request)?;
        framed.push(0);

        let iov = [io::IoSlice::new(&framed)];
        let fds = [reader.as_raw_fd()];
        let cmsgs = [ControlMessage::ScmRights(&fds)];
        sendmsg::<()>(
            self.socket.as_raw_fd(),
            &iov,
            &cmsgs,
            MsgFlags::empty(),
            None,
        )?;
        // Server now owns the reader; drop ours so it sees EOF after the
        // writer closes.
        drop(reader);

        let mut stream = UnixStream::from(writer);
        write_nar(&mut stream)?;
        stream.flush()?;
        drop(stream);

        let reply = self.read_reply()?;
        let path_str = reply
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| VarlinkError::Protocol("missing path in AddToStore reply".into()))?;
        store_dir
            .parse(path_str)
            .map_err(|e| VarlinkError::Protocol(format!("invalid store path '{path_str}': {e}")))
    }

    /// `AddDerivation`. The harmonia `Derivation` is serialized to v4 JSON,
    /// then output method strings are normalized to the spelling PR #13768's
    /// fixtures use (`r` → `nar`).
    pub fn add_derivation(&mut self, store_dir: &StoreDir, drv: &Derivation) -> Result<StorePath> {
        let mut drv_json = serde_json::to_value(drv)?;
        translate_derivation_methods(&mut drv_json);

        let request = json!({
            "method": format!("{INTERFACE}.AddDerivation"),
            "parameters": { "derivation": drv_json },
        });
        let reply = self.call(&request)?;

        let path_str = reply
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| VarlinkError::Protocol("missing path in AddDerivation reply".into()))?;
        store_dir
            .parse(path_str)
            .map_err(|e| VarlinkError::Protocol(format!("invalid store path '{path_str}': {e}")))
    }

    /// `SubmitOutput(name, path)`.
    pub fn submit_output(
        &mut self,
        store_dir: &StoreDir,
        name: &str,
        path: &StorePath,
    ) -> Result<()> {
        let request = json!({
            "method": format!("{INTERFACE}.SubmitOutput"),
            "parameters": {
                "name": name,
                "path": store_dir.display(path).to_string(),
            },
        });
        self.call(&request)?;
        Ok(())
    }

    fn call(&mut self, request: &Value) -> Result<Value> {
        let mut framed = serde_json::to_vec(request)?;
        framed.push(0);
        self.socket.write_all(&framed)?;
        self.read_reply()
    }

    fn read_reply(&mut self) -> Result<Value> {
        self.buf.clear();
        let mut byte = [0u8; 1];
        loop {
            if self.buf.len() >= MAX_REPLY_BYTES {
                return Err(VarlinkError::Protocol(format!(
                    "reply exceeded {MAX_REPLY_BYTES} bytes without null terminator"
                )));
            }
            let n = self.socket.read(&mut byte)?;
            if n == 0 {
                return Err(VarlinkError::Protocol("server closed connection".into()));
            }
            if byte[0] == 0 {
                break;
            }
            self.buf.push(byte[0]);
        }
        let reply: Value = serde_json::from_slice(&self.buf)?;

        if let Some(error) = reply.get("error").and_then(Value::as_str) {
            return Err(map_error(error));
        }
        Ok(reply
            .get("parameters")
            .cloned()
            .unwrap_or_else(|| Value::Object(Default::default())))
    }
}

fn map_error(error: &str) -> VarlinkError {
    let short = error.rsplit('.').next().unwrap_or(error);
    match short {
        "NoFileDescriptor" => VarlinkError::NoFileDescriptor,
        "InvalidNar" => VarlinkError::InvalidNar,
        "InvalidPath" => VarlinkError::InvalidPath,
        "DuplicateOutput" => VarlinkError::DuplicateOutput,
        _ => VarlinkError::Other(error.to_string()),
    }
}

/// Rewrite legacy method spellings in a derivation's outputs map to the
/// v4-format strings PR #13768's fixtures use. Harmonia's `Derivation`
/// emits `{ "method": "r" }` for recursive-NAR CA outputs; the fixtures use
/// `"nar"`. The server's `ContentAddressMethod::parse` happens to accept
/// both today, but we normalize so a server tightening doesn't break us.
fn translate_derivation_methods(drv: &mut Value) {
    let Some(outputs) = drv.get_mut("outputs").and_then(Value::as_object_mut) else {
        return;
    };
    for (_, output) in outputs.iter_mut() {
        let Some(obj) = output.as_object_mut() else {
            continue;
        };
        if let Some(Value::String(method)) = obj.get_mut("method") {
            if method == "r" {
                *method = "nar".to_string();
            }
        }
    }
}

/// Convenience helper: stream `path` (file or directory) as a NAR to the
/// provided writer. For use inside an `add_to_store_nar` closure.
pub fn dump_nar<W: Write>(path: &Path, sink: &mut W) -> io::Result<()> {
    let mut encoder = nix_nar::Encoder::new(path)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("nar encode: {e}")))?;
    io::copy(&mut encoder, sink)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translates_recursive_method() {
        let mut v: Value = serde_json::from_str(
            r#"{"outputs":{"out":{"method":"r","hashAlgo":"sha256"}}}"#,
        )
        .unwrap();
        translate_derivation_methods(&mut v);
        assert_eq!(v["outputs"]["out"]["method"], "nar");
    }

    #[test]
    fn leaves_text_method_alone() {
        let mut v: Value =
            serde_json::from_str(r#"{"outputs":{"out":{"method":"text"}}}"#).unwrap();
        translate_derivation_methods(&mut v);
        assert_eq!(v["outputs"]["out"]["method"], "text");
    }

    #[test]
    fn map_error_strips_interface_prefix() {
        assert!(matches!(
            map_error("org.nix.derivation-builder.InvalidNar"),
            VarlinkError::InvalidNar
        ));
        assert!(matches!(
            map_error("DuplicateOutput"),
            VarlinkError::DuplicateOutput
        ));
        assert!(matches!(
            map_error("Something.Unknown"),
            VarlinkError::Other(_)
        ));
    }
}
