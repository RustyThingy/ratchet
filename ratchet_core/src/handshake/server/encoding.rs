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

use crate::ext::NegotiatedExtension;
use crate::handshake::io::BufferedIo;
use crate::handshake::server::HandshakeResult;
use crate::handshake::{
    get_header, validate_header, validate_header_value, ParseResult, METHOD_GET, UPGRADE_STR,
    WEBSOCKET_STR, WEBSOCKET_VERSION_STR,
};
use crate::handshake::{negotiate_request, TryMap};
use crate::{Error, ErrorKind, HttpError, ProtocolRegistry};
use bytes::{BufMut, BytesMut};
use http::{HeaderMap, StatusCode};
use httparse::Status;
use ratchet_ext::ExtensionProvider;
use tokio::io::AsyncWrite;
use tokio_util::codec::Decoder;

/// The maximum number of headers that will be parsed.
const MAX_HEADERS: usize = 32;
const HTTP_VERSION: &[u8] = b"HTTP/1.1 ";
const STATUS_TERMINATOR_LEN: usize = 2;
const TERMINATOR_NO_HEADERS: &[u8] = b"\r\n\r\n";
const TERMINATOR_WITH_HEADER: &[u8] = b"\r\n";
const HTTP_VERSION_INT: u8 = 1;

pub struct RequestParser<E> {
    pub subprotocols: ProtocolRegistry,
    pub extension: E,
}

impl<E> Decoder for RequestParser<E>
where
    E: ExtensionProvider,
{
    type Item = (HandshakeResult<E::Extension>, usize);
    type Error = Error;

    fn decode(&mut self, buf: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        let RequestParser {
            subprotocols,
            extension,
        } = self;
        let mut headers = [httparse::EMPTY_HEADER; MAX_HEADERS];
        let mut request = httparse::Request::new(&mut headers);

        match try_parse_request(buf, &mut request, extension, subprotocols)? {
            ParseResult::Complete(result, count) => Ok(Some((result, count))),
            ParseResult::Partial => {
                check_partial_request(&request)?;
                Ok(None)
            }
        }
    }
}

pub async fn write_response<S>(
    stream: &mut S,
    buf: &mut BytesMut,
    status: StatusCode,
    headers: HeaderMap,
    body: Option<String>,
) -> Result<(), Error>
where
    S: AsyncWrite + Unpin,
{
    buf.clear();

    let version_count = HTTP_VERSION.len();
    let status_bytes = status.as_str().as_bytes();
    let reason_len = status
        .canonical_reason()
        .map(|r| r.len() + TERMINATOR_NO_HEADERS.len())
        .unwrap_or(TERMINATOR_WITH_HEADER.len());
    let headers_len = headers.iter().fold(0, |count, (name, value)| {
        name.as_str().len() + value.len() + STATUS_TERMINATOR_LEN + count
    });

    let terminator_len = if headers.is_empty() {
        TERMINATOR_NO_HEADERS.len()
    } else {
        TERMINATOR_WITH_HEADER.len()
    };

    buf.reserve(version_count + status_bytes.len() + reason_len + headers_len + terminator_len);

    buf.put_slice(HTTP_VERSION);
    buf.put_slice(status.as_str().as_bytes());

    match status.canonical_reason() {
        Some(reason) => buf.put_slice(format!(" {} \r\n", reason).as_bytes()),
        None => buf.put_slice(b"\r\n"),
    }

    for (name, value) in &headers {
        buf.put_slice(format!("{}: ", name).as_bytes());
        buf.put_slice(value.as_bytes());
        buf.put_slice(b"\r\n");
    }

    if let Some(body) = body {
        buf.put_slice(body.as_bytes());
    }

    if headers.is_empty() {
        buf.put_slice(TERMINATOR_NO_HEADERS);
    } else {
        buf.put_slice(TERMINATOR_WITH_HEADER);
    }

    let mut buffered = BufferedIo::new(stream, buf);
    buffered.write().await
}

pub fn try_parse_request<'l, E>(
    buffer: &'l [u8],
    request: &mut httparse::Request<'_, 'l>,
    extension: E,
    subprotocols: &mut ProtocolRegistry,
) -> Result<ParseResult<HandshakeResult<E::Extension>>, Error>
where
    E: ExtensionProvider,
{
    match request.parse(buffer) {
        Ok(Status::Complete(count)) => {
            parse_request(request, extension, subprotocols).map(|r| ParseResult::Complete(r, count))
        }
        Ok(Status::Partial) => Ok(ParseResult::Partial),
        Err(e) => Err(e.into()),
    }
}

pub fn check_partial_request(request: &httparse::Request) -> Result<(), Error> {
    match request.version {
        Some(HTTP_VERSION_INT) | None => {}
        Some(v) => {
            return Err(Error::with_cause(
                ErrorKind::Http,
                HttpError::HttpVersion(Some(v)),
            ))
        }
    }

    match request.method {
        Some(m) if m.eq_ignore_ascii_case(METHOD_GET) => {}
        None => {}
        m => {
            return Err(Error::with_cause(
                ErrorKind::Http,
                HttpError::HttpMethod(m.map(ToString::to_string)),
            ));
        }
    }

    Ok(())
}

pub fn parse_request<E>(
    request: &mut httparse::Request<'_, '_>,
    extension: E,
    subprotocols: &mut ProtocolRegistry,
) -> Result<HandshakeResult<E::Extension>, Error>
where
    E: ExtensionProvider,
{
    match request.version {
        Some(HTTP_VERSION_INT) => {}
        v => {
            return Err(Error::with_cause(
                ErrorKind::Http,
                HttpError::HttpVersion(v),
            ))
        }
    }

    match request.method {
        Some(m) if m.eq_ignore_ascii_case(METHOD_GET) => {}
        m => {
            return Err(Error::with_cause(
                ErrorKind::Http,
                HttpError::HttpMethod(m.map(ToString::to_string)),
            ));
        }
    }

    let headers = &request.headers;
    validate_header(headers, http::header::CONNECTION, |name, value| {
        let mut parts = value.split(|char| char == &b',' || char == &b' ');
        if parts.any(
            |part| part.eq_ignore_ascii_case(UPGRADE_STR.as_bytes())
        ) {
            Ok(())
        } else {
            Err(Error::with_cause(
                ErrorKind::Http,
                HttpError::InvalidHeader(name),
            ))
        }
    })?;
    validate_header_value(headers, http::header::UPGRADE, WEBSOCKET_STR)?;
    validate_header_value(
        headers,
        http::header::SEC_WEBSOCKET_VERSION,
        WEBSOCKET_VERSION_STR,
    )?;

    validate_header(headers, http::header::HOST, |_, _| Ok(()))?;

    let key = get_header(headers, http::header::SEC_WEBSOCKET_KEY)?;
    let subprotocol = negotiate_request(subprotocols, request)?;
    let extension_opt = extension
        .negotiate_server(request.headers)
        .map_err(|e| Error::with_cause(ErrorKind::Extension, e))?;
    let (extension, extension_header) = match extension_opt {
        Some((extension, header)) => (NegotiatedExtension::from(Some(extension)), Some(header)),
        None => (NegotiatedExtension::from(None), None),
    };

    Ok(HandshakeResult {
        key,
        request: request.try_map()?,
        extension,
        subprotocol,
        extension_header,
    })
}
