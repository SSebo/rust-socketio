use bytes::Bytes;
use futures_util::future::poll_fn;
use futures_util::SinkExt;
use http::Response;
use httparse::{Request, Status, EMPTY_HEADER};
use reqwest::Url;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::{borrow::Cow, net::SocketAddr};
use std::{str::from_utf8, sync::Arc};
use tokio::io::{AsyncReadExt, AsyncWriteExt, ReadBuf};
use tokio::net::TcpStream;
use tokio_tungstenite::{accept_async, MaybeTlsStream, WebSocketStream};
use tungstenite::Message;

use crate::error::Result;
use crate::{Packet, PacketId};

use super::{Server, Sid};

const MAX_BUFF_LEN: usize = 1024;
/// Limit for the number of header lines.
const MAX_HEADERS: usize = 124;

#[derive(Default)]
pub(crate) struct SidGenerator {
    seq: AtomicUsize,
}

impl SidGenerator {
    pub fn generate(&self) -> Sid {
        let seq = self.seq.fetch_add(1, Ordering::SeqCst);
        Arc::new(base64::encode(format!("{}", seq)))
    }
}

pub(crate) struct PollingAcceptor {}

impl PollingAcceptor {
    pub(crate) async fn accept(
        server: Server,
        mut stream: TcpStream,
        addr: &SocketAddr,
    ) -> Result<()> {
        // TODO: polling transport
        match read_request_type(&mut stream, addr).await {
            Some(RequestType::PollingOpen) => {
                let packet = server.handshake_packet(vec!["websocket".to_owned()], None);
                // SAFETY: all fields are safe to serialize
                let data = serde_json::to_string(&packet).unwrap();
                let body = format!("{}{}", PacketId::Open as u8, data);
                write_stream(&mut stream, 200, body).await
            }
            Some(RequestType::PollingGet(_sid)) => {
                write_stream(&mut stream, 200, PacketId::Upgrade.into()).await
            }
            _ => Ok(()),
        }
    }
}

pub(crate) struct WebsocketAcceptor {}

impl WebsocketAcceptor {
    pub(crate) async fn accept(
        server: Server,
        sid: Option<Sid>,
        stream: MaybeTlsStream<TcpStream>,
        addr: &SocketAddr,
    ) -> Result<()> {
        println!("accept websocket {:?}", sid);
        let mut ws_stream = accept_async(stream).await?;
        let sid = match sid {
            // websocket connecting directly, instead of upgrading from polling
            None => handshake(server.clone(), &mut ws_stream).await?,
            Some(sid) => sid,
        };

        server.store_stream(sid, addr, ws_stream).await?;

        Ok(())
    }
}

async fn handshake(
    server: Server,
    ws_stream: &mut WebSocketStream<MaybeTlsStream<TcpStream>>,
) -> Result<Sid> {
    let sid = server.sid();
    let packet = server.handshake_packet(vec![], Some(sid.clone()));
    // SAFETY: all fields are safe to serialize
    let data = serde_json::to_string(&packet).unwrap();
    let message = Message::text(Cow::Borrowed(from_utf8(&Bytes::from(Packet::new(
        PacketId::Open,
        Bytes::from(data),
    )))?));
    ws_stream.send(message).await?;
    Ok(sid)
}

pub(crate) enum RequestType {
    WsUpgrade(Option<Sid>),
    PollingOpen,
    PollingGet(Sid),
    PollingPost(Sid),
}

pub(crate) async fn peek_request_type(
    stream: &TcpStream,
    addr: &SocketAddr,
) -> Option<RequestType> {
    let mut buf = [0; MAX_BUFF_LEN];
    let mut buf = ReadBuf::new(&mut buf);

    poll_fn(|cx| stream.poll_peek(cx, &mut buf)).await.ok()?;
    parse_request_type(buf.filled(), addr)
}

async fn read_request_type(stream: &mut TcpStream, addr: &SocketAddr) -> Option<RequestType> {
    let mut buf = [0; MAX_BUFF_LEN];
    let n = stream.read(&mut buf).await.ok()?;

    parse_request_type(&buf[0..n], addr)
}

pub(crate) fn parse_request_type(buf: &[u8], addr: &SocketAddr) -> Option<RequestType> {
    let mut header_buf = [EMPTY_HEADER; MAX_HEADERS];
    let mut req = Request::new(&mut header_buf);
    let (req, idx) = match req.parse(buf) {
        Ok(Status::Complete(idx)) => (req, idx),
        _ => return None,
    };

    if req.method? != "GET" && req.method? != "POST" {
        return None;
    }

    let mut content_length = 0;
    let url = format!("http://{}{}", addr, req.path?);
    let url = Url::parse(&url).ok()?;
    let mut sid = None;

    for (query_key, query_value) in url.query_pairs() {
        if query_key == "EIO" && query_value != "4" {
            return None;
        }
        if query_key == "sid" {
            sid = Some(Arc::new(query_value.to_string()));
        }
    }

    for header in req.headers {
        if header.name == "Upgrade" && req.method? == "GET" {
            return Some(RequestType::WsUpgrade(sid));
        }

        if header.name == "Content-Length" {
            let len_str = from_utf8(header.value).ok()?;
            content_length = len_str.parse().ok()?;
        }
    }

    if req.method? == "POST" {
        let body_str = from_utf8(&buf[idx..idx + content_length]).ok()?;
        let sid = Arc::new(body_str.to_owned());
        return Some(RequestType::PollingPost(sid));
    }

    match sid {
        Some(sid) => Some(RequestType::PollingGet(sid)),
        _ => Some(RequestType::PollingOpen),
    }
}

async fn write_stream(stream: &mut TcpStream, status: u16, body: String) -> Result<()> {
    let response = http_response(status, body); // not ok, will lost message
    stream.write_all(&Bytes::from(response)).await?;
    Ok(())
}

fn http_response(status: u16, body: String) -> String {
    let response = Response::builder()
        .status(status)
        .header("Content-Type", "text/plain; charset=UTF-8")
        .header("Connection", "Close")
        .header("Content-Length", body.len())
        .body(body);
    // SAFETY: all response fields are valid to build
    let response = response.unwrap();

    let mut response_str = format!(
        "{version:?} {status}\r\n",
        version = response.version(),
        status = response.status()
    );

    for (k, v) in response.headers() {
        // SAFETY: all header value is valid
        let header = format!("{}: {}\r\n", k, v.to_str().unwrap());
        response_str.push_str(&header);
    }

    response_str.push_str("\r\n");
    response_str.push_str(response.body());

    response_str
}
