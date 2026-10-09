//! A fake of Palworld's REST API on loopback, as much of it as a stop and the
//! readiness probe use.
//!
//! `POST /v1/api/save` writes the world and `POST /v1/api/shutdown` exits,
//! each only for `admin` and the password the game was given; `GET
//! /v1/api/info` answers the server's name and version, for the same. It is
//! strict where a sloppy stop would get away with something: a wrong or
//! missing `Authorization` is 401, and a shutdown before a save is 409 and
//! does not exit, so only save-then-shutdown with the right password ends the
//! game with its world saved. The real server (v1.0.5) answered both with 200
//! and an empty body, which is what this answers.
//!
//! Shared, through `#[path]`, by the runner's extension harness (a thread
//! serving it) and the lifecycle tests' fake game (a process serving it),
//! so there is one fake to keep honest.

#![allow(dead_code)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;

/// How the fake behaves.
#[derive(Debug, Clone, Default)]
pub struct Behaviour {
    /// The admin password the game was given.
    pub password: String,
    /// Answer the save with this status instead of saving.
    pub refuse_save: Option<u16>,
}

/// Serve until a shutdown is accepted, then return, as the game exits.
///
/// `seen` gets every request as `<request line> auth=<bool> json=<bool>
/// <body>`, in order; `saved` is called when the world is written.
pub fn serve(
    listener: TcpListener,
    behaviour: &Behaviour,
    seen: &mut dyn FnMut(String),
    saved: &mut dyn FnMut(),
) {
    let expected = format!(
        "Basic {}",
        base64(format!("admin:{}", behaviour.password).as_bytes())
    );
    let mut was_saved = false;
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let mut reader = BufReader::new(stream);
        let mut request_line = String::new();
        if reader.read_line(&mut request_line).is_err() {
            continue;
        }
        let (mut authorized, mut length, mut json) = (false, 0usize, false);
        loop {
            let mut header = String::new();
            if reader.read_line(&mut header).unwrap_or(0) == 0 || header == "\r\n" {
                break;
            }
            let (name, value) = header.trim_end().split_once(": ").unwrap_or_default();
            match name.to_ascii_lowercase().as_str() {
                "authorization" => authorized = value == expected,
                "content-length" => length = value.parse().unwrap_or(0),
                "content-type" => json = value == "application/json",
                _ => {}
            }
        }
        let mut body = vec![0; length];
        if reader.read_exact(&mut body).is_err() {
            continue;
        }
        let body = String::from_utf8_lossy(&body).into_owned();
        let route = request_line.trim_end().to_string();
        seen(format!("{route} auth={authorized} json={json} {body}"));

        let mut answer = String::new();
        let (status, exit) = match route.as_str() {
            _ if !authorized => (401, false),
            "GET /v1/api/info HTTP/1.1" => {
                answer = r#"{"version":"v1.0.5.102999","servername":"Fake","description":"","worldguid":"0"}"#.into();
                (200, false)
            }
            "POST /v1/api/save HTTP/1.1" => match behaviour.refuse_save {
                Some(status) => (status, false),
                None => {
                    saved();
                    was_saved = true;
                    (200, false)
                }
            },
            "POST /v1/api/shutdown HTTP/1.1" if !was_saved => (409, false),
            "POST /v1/api/shutdown HTTP/1.1" => (200, true),
            _ => (404, false),
        };
        let mut stream = reader.into_inner();
        let _ = write!(
            stream,
            "HTTP/1.1 {status} Fake\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{answer}",
            answer.len()
        );
        let _ = stream.flush();
        if exit {
            // Palworld's `waittime`: it answers, then goes a moment later.
            std::thread::sleep(std::time::Duration::from_millis(300));
            return;
        }
    }
}

/// Standard base64, for the expected `Authorization`.
fn base64(input: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            out.push(if i <= chunk.len() {
                ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char
            } else {
                '='
            });
        }
    }
    out
}
