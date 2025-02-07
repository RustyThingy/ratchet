// Copyright 2015-2021 Swim Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

#[cfg(test)]
mod tests;

mod client;
mod io;
mod server;
mod subprotocols;

use crate::errors::Error;
use crate::errors::{ErrorKind, HttpError};
use crate::handshake::io::BufferedIo;
use crate::{InvalidHeader, Request};
use bytes::Bytes;
use http::header::HeaderName;
use http::Uri;
use http::{HeaderMap, HeaderValue};
use std::str::FromStr;
use tokio::io::AsyncRead;
use tokio_util::codec::Decoder;
use url::Url;

pub use client::{subscribe, subscribe_with, HandshakeResult, UpgradedClient};
pub use server::{accept, accept_with, UpgradedServer, WebSocketResponse, WebSocketUpgrader};
pub use subprotocols::*;

const WEBSOCKET_STR: &str = "websocket";
const UPGRADE_STR: &str = "upgrade";
const WEBSOCKET_VERSION_STR: &str = "13";
const BAD_STATUS_CODE: &str = "Invalid status code";
const ACCEPT_KEY: &[u8] = b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
const METHOD_GET: &str = "get";

pub struct StreamingParser<'i, 'buf, I, P> {
    io: &'i mut BufferedIo<'buf, I>,
    parser: P,
}

impl<'i, 'buf, I, P, O> StreamingParser<'i, 'buf, I, P>
where
    I: AsyncRead + Unpin,
    P: Decoder<Item = (O, usize), Error = Error>,
{
    pub fn new(io: &'i mut BufferedIo<'buf, I>, parser: P) -> StreamingParser<'i, 'buf, I, P> {
        StreamingParser { io, parser }
    }

    pub async fn parse(self) -> Result<O, Error> {
        let StreamingParser { io, mut parser } = self;

        loop {
            io.read().await?;

            match parser.decode(io.buffer) {
                Ok(Some((out, count))) => {
                    io.advance(count);
                    return Ok(out);
                }
                Ok(None) => continue,
                Err(e) => return Err(e),
            }
        }
    }
}

pub enum ParseResult<O> {
    Complete(O, usize),
    Partial,
}

/// A trait for creating a request from a type.
pub trait TryIntoRequest {
    /// Attempt to convert this type into a `Request`.
    fn try_into_request(self) -> Result<Request, Error>;
}

impl<'a> TryIntoRequest for &'a str {
    fn try_into_request(self) -> Result<Request, Error> {
        self.parse::<Uri>()?.try_into_request()
    }
}

impl<'a> TryIntoRequest for &'a String {
    fn try_into_request(self) -> Result<Request, Error> {
        self.as_str().try_into_request()
    }
}

impl TryIntoRequest for String {
    fn try_into_request(self) -> Result<Request, Error> {
        self.as_str().try_into_request()
    }
}

impl<'a> TryIntoRequest for &'a Uri {
    fn try_into_request(self) -> Result<Request, Error> {
        self.clone().try_into_request()
    }
}

impl TryIntoRequest for Uri {
    fn try_into_request(self) -> Result<Request, Error> {
        Ok(Request::get(self).body(())?)
    }
}

impl<'a> TryIntoRequest for &'a Url {
    fn try_into_request(self) -> Result<Request, Error> {
        self.as_str().try_into_request()
    }
}

impl TryIntoRequest for Url {
    fn try_into_request(self) -> Result<Request, Error> {
        self.as_str().try_into_request()
    }
}

impl TryIntoRequest for Request {
    fn try_into_request(self) -> Result<Request, Error> {
        Ok(self)
    }
}

fn validate_header_value(
    headers: &[httparse::Header],
    name: HeaderName,
    expected: &str,
) -> Result<(), Error> {
    validate_header(headers, name, |name, actual| {
        if actual.eq_ignore_ascii_case(expected.as_bytes()) {
            Ok(())
        } else {
            Err(Error::with_cause(
                ErrorKind::Http,
                HttpError::InvalidHeader(name),
            ))
        }
    })
}

fn validate_header<F>(headers: &[httparse::Header], name: HeaderName, f: F) -> Result<(), Error>
where
    F: Fn(HeaderName, &[u8]) -> Result<(), Error>,
{
    match headers
        .iter()
        .find(|h| h.name.eq_ignore_ascii_case(name.as_str()))
    {
        Some(header) => f(name, header.value),
        None => Err(Error::with_cause(
            ErrorKind::Http,
            HttpError::MissingHeader(name),
        )),
    }
}

fn get_header(headers: &[httparse::Header], name: HeaderName) -> Result<Bytes, Error> {
    match headers
        .iter()
        .find(|h| h.name.eq_ignore_ascii_case(name.as_str()))
    {
        Some(header) => Ok(Bytes::from(header.value.to_vec())),
        None => Err(Error::with_cause(
            ErrorKind::Http,
            HttpError::MissingHeader(name),
        )),
    }
}

/// Local replacement for TryInto that can be implemented for httparse::Header and httparse::Request
pub trait TryMap<Target> {
    /// Error type returned if the mapping fails
    type Error: Into<Error>;

    /// Try and map this into `Target`
    fn try_map(self) -> Result<Target, Self::Error>;
}

impl<'h> TryMap<HeaderMap> for &'h [httparse::Header<'h>] {
    type Error = InvalidHeader;

    fn try_map(self) -> Result<HeaderMap, Self::Error> {
        let mut header_map = HeaderMap::with_capacity(self.len());
        for header in self {
            let header_string = || {
                let value = String::from_utf8_lossy(header.value);
                format!("{}: {}", header.name, value)
            };

            let name =
                HeaderName::from_str(header.name).map_err(|_| InvalidHeader(header_string()))?;
            let value = HeaderValue::from_bytes(header.value)
                .map_err(|_| InvalidHeader(header_string()))?;
            header_map.insert(name, value);
        }

        Ok(header_map)
    }
}

impl<'l, 'h, 'buf: 'h> TryMap<Request> for &'l httparse::Request<'h, 'buf> {
    type Error = HttpError;

    fn try_map(self) -> Result<Request, Self::Error> {
        let mut request = Request::new(());
        let path = match self.path {
            Some(path) => path.parse()?,
            None => return Err(HttpError::MalformattedUri(None)),
        };
        let headers = &self.headers;

        *request.headers_mut() = headers.try_map()?;
        *request.uri_mut() = path;

        Ok(request)
    }
}
