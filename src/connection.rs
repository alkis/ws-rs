use std::mem::replace;
use std::mem::transmute;
use std::borrow::Borrow;
use std::io::{Write, Read, Cursor, Seek, SeekFrom};
use std::net::SocketAddr;
use std::collections::VecDeque;
use std::str::from_utf8;

use url;
use mio::{Token, TryRead, TryWrite, EventSet};
use mio::tcp::TcpStream;
#[cfg(all(not(windows), feature="ssl"))]
use openssl::ssl::NonblockingSslStream;

use message::Message;
use handshake::{Handshake, Request, Response};
use frame::Frame;
use protocol::{CloseCode, OpCode};
use result::{Result, Error, Kind};
use handler::Handler;
use stream::Stream;

use self::State::*;
use self::Endpoint::*;

use super::Settings;

#[derive(Debug)]
pub enum State {
    // Tcp connection accepted, waiting for handshake to complete
    Connecting(Cursor<Vec<u8>>, Cursor<Vec<u8>>),
    // Ready to send/receive messages
    Open,
    // Close frame sent/received
    Closing,
}

/// A little more semantic than a boolean
#[derive(Debug, Eq, PartialEq, Clone, Copy)]
pub enum Endpoint {
    /// Will mask outgoing frames
    Client,
    /// Won't mask outgoing frames
    Server,
}

impl State {

    #[inline]
    pub fn is_connecting(&self) -> bool {
        match *self {
            State::Connecting(..) => true,
            _ => false,
        }
    }

    #[allow(dead_code)]
    #[inline]
    pub fn is_open(&self) -> bool {
        match *self {
            State::Open => true,
            _ => false,
        }
    }

    #[inline]
    pub fn is_closing(&self) -> bool {
        match *self {
            State::Closing => true,
            _ => false,
        }
    }
}

pub struct Connection<H>
    where H: Handler
{
    token: Token,
    socket: Stream,
    state: State,
    endpoint: Endpoint,
    events: EventSet,

    fragments: VecDeque<Frame>,

    in_buffer: Cursor<Vec<u8>>,
    out_buffer: Cursor<Vec<u8>>,

    handler: H,

    addresses: Vec<SocketAddr>,

    settings: Settings,
}

impl<H> Connection<H>
    where H: Handler
{
    pub fn new(tok: Token, sock: TcpStream, handler: H, settings: Settings) -> Connection<H> {
        Connection {
            token: tok,
            socket: Stream::tcp(sock),
            state: Connecting(
                Cursor::new(Vec::with_capacity(2048)),
                Cursor::new(Vec::with_capacity(2048)),
            ),
            endpoint: Endpoint::Server,
            events: EventSet::hup(),
            fragments: VecDeque::with_capacity(settings.fragments_capacity),
            in_buffer: Cursor::new(Vec::with_capacity(settings.in_buffer_capacity)),
            out_buffer: Cursor::new(Vec::with_capacity(settings.out_buffer_capacity)),
            handler: handler,
            addresses: Vec::new(),
            settings: settings,
        }
    }

    pub fn as_server(&mut self) -> Result<()> {
        Ok(self.events.insert(EventSet::readable()))
    }

    pub fn as_client(&mut self, url: &url::Url, addrs: Vec<SocketAddr>) -> Result<()> {
        if let Connecting(ref mut req, _) = self.state {
            self.addresses = addrs;
            self.events.insert(EventSet::writable());
            self.endpoint = Endpoint::Client;
            try!(self.handler.build_request(url)).format(req.get_mut())
        } else {
            Err(Error::new(
                Kind::Internal,
                "Tried to set connection to client while not connecting."))
        }
    }

    #[cfg(all(not(windows), feature="ssl"))]
    pub fn encrypt(&mut self) -> Result<()> {
        let ssl_stream = match self.endpoint {
            Server => try!(NonblockingSslStream::accept(
                try!(self.handler.build_ssl()),
                try!(self.socket().try_clone()))),

            Client => try!(NonblockingSslStream::connect(
                try!(self.handler.build_ssl()),
                try!(self.socket().try_clone()))),
        };

        Ok(self.socket = Stream::tls(ssl_stream))
    }

    pub fn token(&self) -> Token {
        self.token
    }

    pub fn socket(&self) -> &TcpStream {
        &self.socket.evented()
    }

    // Resetting may be necessary in order to try all possible addresses for a server
    #[cfg(all(not(windows), feature="ssl"))]
    pub fn reset(&mut self) -> Result<()> {
        if self.is_client() {
            if let Connecting(ref mut req, ref mut res) = self.state {
                req.set_position(0);
                res.set_position(0);
                self.events.remove(EventSet::readable());
                self.events.insert(EventSet::writable());

                if let Some(ref addr) = self.addresses.pop() {
                    let sock = try!(TcpStream::connect(addr));
                    if self.socket.is_tls() {
                        Ok(self.socket = Stream::tls(
                                try!(NonblockingSslStream::connect(
                                    try!(self.handler.build_ssl()),
                                    sock))))

                    } else {
                        Ok(self.socket = Stream::tcp(sock))
                    }
                } else {
                    if self.settings.panic_on_new_connection {
                        panic!("Unable to connect to server.");
                    }
                    Err(Error::new(Kind::Internal, "Unable to connect to server."))
                }
            } else {
                Err(Error::new(Kind::Internal, "Unable to reset client connection because it is active."))
            }
        } else {
            Err(Error::new(Kind::Internal, "Server connections cannot be reset."))
        }
    }

    #[cfg(not(feature="ssl"))]
    pub fn reset(&mut self) -> Result<()> {
        if self.is_client() {
            if let Connecting(ref mut req, ref mut res) = self.state {
                req.set_position(0);
                res.set_position(0);
                self.events.remove(EventSet::readable());
                self.events.insert(EventSet::writable());

                if let Some(ref addr) = self.addresses.pop() {
                    let sock = try!(TcpStream::connect(addr));
                    Ok(self.socket = Stream::tcp(sock))
                } else {
                    if self.settings.panic_on_new_connection {
                        panic!("Unable to connect to server.");
                    }
                    Err(Error::new(Kind::Internal, "Unable to connect to server."))
                }
            } else {
                Err(Error::new(Kind::Internal, "Unable to reset client connection because it is active."))
            }
        } else {
            Err(Error::new(Kind::Internal, "Server connections cannot be reset."))
        }
    }

    pub fn state(&mut self) -> &mut State {
        &mut self.state
    }

    pub fn events(&self) -> EventSet {
        self.events
    }

    pub fn is_client(&self) -> bool {
        match self.endpoint {
            Client => true,
            Server => false,
        }
    }

    pub fn is_server(&self) -> bool {
        match self.endpoint {
            Client => false,
            Server => true,
        }
    }

    pub fn shutdown(&mut self) {
        self.handler.on_shutdown();
        if let Err(err) = self.send_close(CloseCode::Away, "Shutting down.") {
            self.handler.on_error(err);
            self.events = EventSet::none();
        }
    }

    pub fn error(&mut self, err: Error) {
        match self.state {
            Connecting(_, ref mut res) => {
                match err.kind {
                    #[cfg(all(not(windows), feature="ssl"))]
                    Kind::Ssl(_) | Kind::Io(_) => {
                        self.handler.on_error(err);
                        self.events = EventSet::none();
                    }
                    Kind::Protocol => {
                        let msg = err.to_string();
                        self.handler.on_error(err);
                        if let Server = self.endpoint {
                            res.get_mut().clear();
                            if let Err(err) = write!(
                                    res.get_mut(),
                                    "HTTP/1.1 400 Bad Request\r\n\r\n{}", msg) {
                                self.handler.on_error(Error::from(err));
                                self.events = EventSet::none();
                            } else {
                                self.events.remove(EventSet::readable());
                                self.events.insert(EventSet::writable());
                            }
                        } else {
                            self.events = EventSet::none();
                        }

                    }
                    _ => {
                        let msg = err.to_string();
                        self.handler.on_error(err);
                        if let Server = self.endpoint {
                            res.get_mut().clear();
                            if let Err(err) = write!(
                                    res.get_mut(),
                                    "HTTP/1.1 500 Internal Server Error\r\n\r\n{}", msg) {
                                self.handler.on_error(Error::from(err));
                                self.events = EventSet::none();
                            } else {
                                self.events.remove(EventSet::readable());
                                self.events.insert(EventSet::writable());
                            }
                        } else {
                            self.events = EventSet::none();
                        }
                    }
                }

            }
            _ => {
                match err.kind {
                    Kind::Internal => {
                        if self.settings.panic_on_internal {
                            panic!("Panicking on internal error -- {}", err);
                        }
                        let reason = format!("{}", err);

                        self.handler.on_error(err);
                        if let Err(err) = self.send_close(CloseCode::Error, reason) {
                            self.handler.on_error(err);
                            self.events = EventSet::none();
                        }
                    }
                    Kind::Capacity => {
                        if self.settings.panic_on_capacity {
                            panic!("Panicking on capacity error -- {}", err);
                        }
                        let reason = format!("{}", err);

                        self.handler.on_error(err);
                        if let Err(err) = self.send_close(CloseCode::Size, reason) {
                            self.handler.on_error(err);
                            self.events = EventSet::none();
                        }
                    }
                    Kind::Protocol => {
                        if self.settings.panic_on_protocol {
                            panic!("Panicking on protocol error -- {}", err);
                        }
                        let reason = format!("{}", err);

                        self.handler.on_error(err);
                        if let Err(err) = self.send_close(CloseCode::Protocol, reason) {
                            self.handler.on_error(err);
                            self.events = EventSet::none();
                        }
                    }
                    Kind::Encoding(_) => {
                        if self.settings.panic_on_encoding {
                            panic!("Panicking on encoding error -- {}", err);
                        }
                        let reason = format!("{}", err);

                        self.handler.on_error(err);
                        if let Err(err) = self.send_close(CloseCode::Invalid, reason) {
                            self.handler.on_error(err);
                            self.events = EventSet::none();
                        }
                    }
                    Kind::Parse(_) => {
                        debug_assert!(false, "Encountered HTTP parse error while not in connecting state!");
                        error!("Encountered HTTP parse error while not in connecting state!");
                        self.handler.on_error(err);
                        error!("Disconnecting WebSocket.");
                        self.events = EventSet::none();
                    }
                    Kind::Custom(_) => {
                        self.handler.on_error(err);
                    }
                    _ => {
                        if self.settings.panic_on_io {
                            panic!("Panicking on io error -- {}", err);
                        }
                        self.handler.on_error(err);
                        self.events = EventSet::none();
                    }
                }
            }
        }
    }

    fn write_handshake(&mut self) -> Result<()> {
        if let Connecting(ref mut req, ref mut res) = self.state {
            match self.endpoint {
                Server => {
                    let mut done = false;
                    if let Some(len) = try!(self.socket.try_write_buf(res)) {
                        if res.get_ref().len() == len {
                            done = true
                        }
                    }
                    if !done {
                        return Ok(())
                    }
                }
                Client =>  {
                    if let Some(len) = try!(self.socket.try_write_buf(req)) {
                        if req.get_ref().len() == len {
                            debug!("Finished writing handshake request to {}", try!(self.socket.peer_addr()));
                            self.events.insert(EventSet::readable());
                            self.events.remove(EventSet::writable());
                        }
                    }
                    return Ok(())
                }
            }
        }

        if let Connecting(ref req, ref res) = replace(&mut self.state, Open) {
            debug!("Finished writing handshake response to {}", try!(self.socket.peer_addr()));
            debug!("Connection to {} is now open.", try!(self.socket.peer_addr()));

            let request = try!(try!(Request::parse(req.get_ref())).ok_or(
                Error::new(Kind::Internal, "Failed to parse request after handshake is complete.")));

            let response = try!(try!(Response::parse(res.get_ref())).ok_or(
                Error::new(Kind::Internal, "Failed to parse response after handshake is complete.")));

            if response.status() != 101 {
                if response.status() != 301 && response.status() != 302 {
                    return Err(Error::new(Kind::Protocol, "Handshake failed."));
                } else {
                    self.events.insert(EventSet::readable());
                    self.events.remove(EventSet::writable());
                    return Ok(())
                }
            } else {
                try!(self.handler.on_open(Handshake {
                    request: request,
                    response: response,
                    peer_addr: self.socket.peer_addr().ok(),
                    local_addr: self.socket.local_addr().ok(),
                }));
                self.events.insert(EventSet::readable());
                return Ok(self.check_events())
            }
        } else {
            Err(Error::new(Kind::Internal, "Tried to write WebSocket handshake while not in connecting state!"))
        }
    }

    fn read_handshake(&mut self) -> Result<()> {
        if let Connecting(ref mut req, ref mut res) = self.state {
            match self.endpoint {
                Server => {
                    if let Some(_) = try!(self.socket.try_read_buf(req.get_mut())) {
                        if let Some(ref request) = try!(Request::parse(req.get_ref())) {
                            debug!("Handshake request received: \n{}", request);
                            let response = try!(self.handler.on_request(request));
                            try!(response.format(res.get_mut()));
                            self.events.remove(EventSet::readable());
                            self.events.insert(EventSet::writable());
                        }
                    }
                    return Ok(())
                }
                Client => {
                    if let Some(_) = try!(self.socket.try_read_buf(res.get_mut())) {

                        // TODO: see if this can be optimized with drain
                        let end = {
                            let data = res.get_ref();
                            let end = data.iter()
                                          .enumerate()
                                          .take_while(|&(ind, _)| !data[..ind].ends_with(b"\r\n\r\n"))
                                          .count();
                            if !data[..end].ends_with(b"\r\n\r\n") {
                                return Ok(())
                            }
                            self.in_buffer.get_mut().extend(&data[end..]);
                            end
                        };
                        res.get_mut().truncate(end);
                    }
                }
            }
        }

        if let Connecting(ref req, ref res) = replace(&mut self.state, Open) {
            debug!("Finished reading handshake response from {}", try!(self.socket.peer_addr()));
            debug!("Connection to {} is now open.", try!(self.socket.peer_addr()));

            let request = try!(try!(Request::parse(req.get_ref())).ok_or(
                Error::new(Kind::Internal, "Failed to parse request after handshake is complete.")));

            let response = try!(try!(Response::parse(res.get_ref())).ok_or(
                Error::new(Kind::Internal, "Failed to parse response after handshake is complete.")));

            debug!("Handshake response received: \n{}", response);

            if response.status() != 101 {
                if response.status() != 301 && response.status() != 302 {
                    return Err(Error::new(Kind::Protocol, "Handshake failed."));
                } else {
                    return Ok(())
                }
            }

            if self.settings.key_strict {
                let req_key = try!(request.hashed_key());
                let res_key = try!(from_utf8(try!(response.key())));
                if req_key != res_key {
                    return Err(Error::new(Kind::Protocol, format!("Received incorrect WebSocket Accept key: {} vs {}", req_key, res_key)));
                }
            }

            try!(self.handler.on_response(&response));
            try!(self.handler.on_open(Handshake {
                    request: request,
                    response: response,
                    peer_addr: self.socket.peer_addr().ok(),
                    local_addr: self.socket.local_addr().ok(),
            }));

            // check to see if there is anything to read already
            if !self.in_buffer.get_ref().is_empty() {
                try!(self.read_frames());
            }

            return Ok(self.check_events())
        }
        Err(Error::new(Kind::Internal, "Tried to read WebSocket handshake while not in connecting state!"))
    }

    pub fn read(&mut self) -> Result<()> {
        if self.socket.is_negotiating() {
            try!(self.socket.clear_negotiating());
            self.write()
        } else {
            let res = if self.state.is_connecting() {
                debug!("Ready to read handshake from {}.", try!(self.socket.peer_addr()));
                self.read_handshake()
            } else {
                debug!("Ready to read messages from {}.", try!(self.socket.peer_addr()));
                if let Some(_) = try!(self.buffer_in()) {
                    self.read_frames()
                } else {
                    Ok(())
                }
            };

            if self.socket.is_negotiating() && res.is_ok() {
                self.events.remove(EventSet::readable());
                self.events.insert(EventSet::writable());
            }
            res
        }
    }

    fn read_frames(&mut self) -> Result<()> {
        while let Some(mut frame) = try!(Frame::parse(&mut self.in_buffer)) {

            if self.settings.masking_strict {
                if frame.is_masked() {
                    if self.is_client() {
                        return Err(Error::new(Kind::Protocol, "Received masked frame from a server endpoint."))
                    }
                } else {
                    if self.is_server() {
                        return Err(Error::new(Kind::Protocol, "Received unmasked frame from a client endpoint."))
                    }
                }
            }

            // This is safe whether or not a frame is masked.
            frame.remove_mask();

            if frame.is_final() {
                match frame.opcode() {
                    // singleton data frames
                    OpCode::Text => {
                        debug!("Received text frame {:?}", frame);
                        if let Some(frame) = try!(self.handler.on_frame(frame)) {
                            // since we are going to handle this, there can't be an ongoing
                            // message
                            if !self.fragments.is_empty() {
                                return Err(Error::new(Kind::Protocol, "Received unfragmented text frame while processing fragmented message."))
                            }
                            debug_assert!(frame.opcode() == OpCode::Text, "Handler passed back corrupted frame.");
                            let msg = Message::text(try!(String::from_utf8(frame.into_data()).map_err(|err| err.utf8_error())));
                            try!(self.handler.on_message(msg));
                        }
                    }
                    OpCode::Binary => {
                        debug!("Received binary frame {:?}", frame);
                        if let Some(frame) = try!(self.handler.on_frame(frame)) {
                            // since we are going to handle this, there can't be an ongoing
                            // message
                            if !self.fragments.is_empty() {
                                return Err(Error::new(Kind::Protocol, "Received unfragmented binary frame while processing fragmented message."))
                            }
                            debug_assert!(frame.opcode() == OpCode::Binary, "Handler passed back corrupted frame.");
                            let data = frame.into_data();
                            try!(self.handler.on_message(Message::binary(data)));
                        }
                    }
                    // control frames
                    OpCode::Close => {
                        debug!("Received close frame {:?}", frame);
                        if !self.state.is_closing() {
                            if let Some(frame) = try!(self.handler.on_frame(frame)) {
                                debug_assert!(frame.opcode() == OpCode::Close, "Handler passed back corrupted frame.");

                                let mut close_code = [0u8; 2];
                                let mut data = Cursor::new(frame.into_data());
                                if let 2 = try!(data.read(&mut close_code)) {
                                    let code_be: u16 = unsafe {transmute(close_code) };
                                    debug!("Connection to {} received raw close code: {:?}, {:b}", try!(self.socket.peer_addr()), code_be, code_be);
                                    let named = CloseCode::from(u16::from_be(code_be));
                                    if let CloseCode::Other(code) = named {
                                        if
                                                code < 1000 ||
                                                code >= 5000 ||
                                                code == 1004 ||
                                                code == 1014 ||
                                                code == 1016 || // these below are here to pass the autobahn test suite
                                                code == 1100 || // we shouldn't need them later
                                                code == 2000 ||
                                                code == 2999
                                        {
                                            return Err(Error::new(Kind::Protocol, format!("Received invalid close code from endpoint: {}", code)))
                                        }
                                    }
                                    let has_reason = {
                                        if let Ok(reason) = from_utf8(&data.get_ref()[2..]) {
                                            self.handler.on_close(named, reason); // note reason may be an empty string
                                            true
                                        } else {
                                            self.handler.on_close(named, "");
                                            false
                                        }
                                    };

                                    if let CloseCode::Abnormal = named {
                                        return Err(Error::new(Kind::Protocol, "Received abnormal close code from endpoint."))
                                    } else if let CloseCode::Status = named {
                                        return Err(Error::new(Kind::Protocol, "Received no status close code from endpoint."))
                                    } else if let CloseCode::Restart = named {
                                        return Err(Error::new(Kind::Protocol, "Restart close code is not supported."))
                                    } else if let CloseCode::Again = named {
                                        return Err(Error::new(Kind::Protocol, "Try again later close code is not supported."))
                                    } else if let CloseCode::Tls = named {
                                        return Err(Error::new(Kind::Protocol, "Received TLS close code outside of TLS handshake."))
                                    } else {
                                        if has_reason {
                                            try!(self.send_close(named, "")); // note this drops any extra close data
                                        } else {
                                            try!(self.send_close(CloseCode::Invalid, ""));
                                        }
                                    }
                                } else {
                                    // This is not an error. It is allowed behavior in the
                                    // protocol, so we don't trigger an error
                                    self.handler.on_close(CloseCode::Status, "Unable to read close code. Sending empty close frame.");
                                    try!(self.send_close(CloseCode::Empty, ""));
                                }
                            }
                        }
                    }
                    OpCode::Ping => {
                        debug!("Received ping frame {:?}", frame);
                        if let Some(frame) = try!(self.handler.on_frame(frame)) {
                            debug_assert!(frame.opcode() == OpCode::Ping, "Handler passed back corrupted frame.");
                            try!(self.send_pong(frame.into_data()));
                        }
                    }
                    OpCode::Pong => {
                        debug!("Received pong frame {:?}", frame);
                        // no ping validation for now
                        try!(self.handler.on_frame(frame));
                    }
                    // last fragment
                    OpCode::Continue => {
                        debug!("Received final fragment {:?}", frame);
                        if let Some(last) = try!(self.handler.on_frame(frame)) {
                            if let Some(first) = self.fragments.pop_front() {
                                let size = self.fragments.iter().fold(first.payload().len() + last.payload().len(), |len, frame| len + frame.payload().len());
                                match first.opcode() {
                                    OpCode::Text => {
                                        debug!("Constructing text message from fragments: {:?} -> {:?} -> {:?}", first, self.fragments.iter().collect::<Vec<&Frame>>(), last);
                                        let mut data = Vec::with_capacity(size);
                                        data.extend(first.into_data());
                                        while let Some(frame) = self.fragments.pop_front() {
                                            data.extend(frame.into_data());
                                        }
                                        data.extend(last.into_data());

                                        let string = try!(String::from_utf8(data).map_err(|err| err.utf8_error()));

                                        debug!("Calling handler with constructed message: {:?}", string);
                                        try!(self.handler.on_message(Message::text(string)));
                                    }
                                    OpCode::Binary => {
                                        debug!("Constructing text message from fragments: {:?} -> {:?} -> {:?}", first, self.fragments.iter().collect::<Vec<&Frame>>(), last);
                                        let mut data = Vec::with_capacity(size);
                                        data.extend(first.into_data());

                                        while let Some(frame) = self.fragments.pop_front() {
                                            data.extend(frame.into_data());
                                        }

                                        data.extend(last.into_data());

                                        debug!("Calling handler with constructed message: {:?}", data);
                                        try!(self.handler.on_message(Message::binary(data)));
                                    }
                                    _ => {
                                        return Err(Error::new(Kind::Protocol, "Encounted fragmented control frame."))
                                    }
                                }
                            } else {
                                return Err(Error::new(Kind::Protocol, "Unable to reconstruct fragmented message. No first frame."))
                            }
                        }
                    }
                    _ => return Err(Error::new(Kind::Protocol, "Encountered invalid opcode.")),
                }
            } else {
                match frame.opcode() {
                    OpCode::Text | OpCode::Binary | OpCode::Continue => {
                        debug!("Received non-final fragment frame {:?}", frame);
                        if let Some(frame) = try!(self.handler.on_frame(frame)) {
                            self.fragments.push_back(frame)
                        }
                    }
                    _ => {
                        return Err(Error::new(Kind::Protocol, "Encounted fragmented control frame."))
                    }
                }
            }
        }
        Ok(())
    }

    pub fn write(&mut self) -> Result<()> {
        if self.socket.is_negotiating() {
            try!(self.socket.clear_negotiating());
            self.read()
        } else {
            let res = if self.state.is_connecting() {
                debug!("Ready to write handshake to {}.", try!(self.socket.peer_addr()));
                self.write_handshake()
            } else {
                debug!("Ready to write messages to {}.", try!(self.socket.peer_addr()));

                // Start out assuming that this write will clear the whole buffer
                self.events.remove(EventSet::writable());

                while let Some(len) = try!(self.socket.try_write_buf(&mut self.out_buffer)) {
                    debug!("Wrote {} bytes to {}", len, try!(self.socket.peer_addr()));
                    let finished = len == 0 || self.out_buffer.position() as usize == self.out_buffer.get_ref().len();
                    if finished && self.is_server() && self.state.is_closing() {
                        // we are are a server that is closing and just wrote out our last frame,
                        // let's disconnect
                        return Ok(self.events = EventSet::none());
                    } else if finished {
                        break
                    }
                }

                // Check if there is more to write so that the connection will be rescheduled
                Ok(self.check_events())
            };

            if self.socket.is_negotiating() && res.is_ok() {
                self.events.remove(EventSet::writable());
                self.events.insert(EventSet::readable());
            }
            res
        }
    }

    pub fn send_message(&mut self, msg: Message) -> Result<()> {
        let opcode = msg.opcode();
        debug!("Message opcode {:?}", opcode);
        let data = msg.into_data();
        if data.len() > self.settings.fragment_size {
            debug!("Chunking at {:?}.", self.settings.fragment_size);
            // note this copies the data, so it's actually somewhat expensive to fragment
            let mut chunks = data.chunks(self.settings.fragment_size).peekable();
            let chunk = chunks.next().expect("Unable to get initial chunk!");

            try!(self.buffer_frame(
                Frame::message(Vec::from(chunk), opcode, false)));

            while let Some(chunk) = chunks.next() {
                if let Some(_) = chunks.peek() {
                    try!(self.buffer_frame(
                        Frame::message(Vec::from(chunk), OpCode::Continue, false)));
                } else {
                    try!(self.buffer_frame(
                        Frame::message(Vec::from(chunk), OpCode::Continue, true)));
                }
            }

        } else {
            debug!("Sending unfragmented message frame.");
            // true means that the message is done
            try!(self.buffer_frame(Frame::message(data, opcode, true)));
        }
        Ok(self.check_events())
    }

    #[inline]
    pub fn send_ping(&mut self, data: Vec<u8>) -> Result<()> {
        debug!("Sending ping to {}.", try!(self.socket.peer_addr()));
        try!(self.buffer_frame(Frame::ping(data)));
        Ok(self.check_events())
    }

    #[inline]
    pub fn send_pong(&mut self, data: Vec<u8>) -> Result<()> {
        if self.state.is_closing() {
            return Ok(())
        }
        debug!("Sending pong to {}.", try!(self.socket.peer_addr()));
        try!(self.buffer_frame(Frame::pong(data)));
        Ok(self.check_events())
    }

    #[inline]
    pub fn send_close<R>(&mut self, code: CloseCode, reason: R) -> Result<()>
        where R: Borrow<str>
    {
        debug!("Sending close {:?} -- {:?} to {}.", code, reason.borrow(), try!(self.socket.peer_addr()));
        try!(self.buffer_frame(Frame::close(code, reason.borrow())));

        debug!("Connection to {} is now closing.", try!(self.socket.peer_addr()));
        self.state = Closing;
        Ok(self.check_events())
    }

    fn check_events(&mut self) {
        if !self.state.is_connecting() {
            self.events.insert(EventSet::readable());
            if (self.out_buffer.position() as usize) < self.out_buffer.get_ref().len() {
                self.events.insert(EventSet::writable());
            }
        }
    }

    fn buffer_frame(&mut self, frame: Frame) -> Result<()> {
        if let Some(mut frame) = try!(self.handler.on_send_frame(frame)) {
            try!(self.check_buffer_out(&frame));

            if self.is_client() {
                frame.set_mask();
            }

            debug!("Buffering frame to {}:\n{}", try!(self.socket.peer_addr()), frame);

            let pos = self.out_buffer.position();
            try!(self.out_buffer.seek(SeekFrom::End(0)));
            try!(frame.format(&mut self.out_buffer));
            try!(self.out_buffer.seek(SeekFrom::Start(pos)));
        }
        Ok(())
    }

    fn check_buffer_out(&mut self, frame: &Frame) -> Result<()> {

        if self.out_buffer.get_ref().capacity() <= self.out_buffer.get_ref().len() + frame.len() {
            // extend
            let mut new = Vec::with_capacity(self.out_buffer.get_ref().capacity());
            new.extend(&self.out_buffer.get_ref()[self.out_buffer.position() as usize ..]);
            if new.len() == new.capacity() {
                if self.settings.out_buffer_grow {
                    new.reserve(self.settings.out_buffer_capacity)
                } else {
                    return Err(Error::new(Kind::Capacity, "Maxed out output buffer for connection."))
                }
            }
            self.out_buffer = Cursor::new(new);
        }
        Ok(())
    }

    fn buffer_in(&mut self) -> Result<Option<usize>> {

        debug!("Reading buffer for connection to {}.", try!(self.socket.peer_addr()));
        if let Some(mut len) = try!(self.socket.try_read_buf(self.in_buffer.get_mut())) {
            if len == 0 {
                debug!("Buffered {}.", len);
                return Ok(None)
            }
            loop {
                if self.in_buffer.get_ref().len() == self.in_buffer.get_ref().capacity() {
                    // extend
                    let mut new = Vec::with_capacity(self.in_buffer.get_ref().capacity());
                    new.extend(&self.in_buffer.get_ref()[self.in_buffer.position() as usize ..]);
                    if new.len() == new.capacity() {
                        if self.settings.in_buffer_grow {
                            new.reserve(self.settings.in_buffer_capacity);
                        } else {
                            return Err(Error::new(Kind::Capacity, "Maxed out input buffer for connection."))
                        }

                        self.in_buffer = Cursor::new(new);
                        // return now so that hopefully we will consume some of the buffer so this
                        // won't happen next time
                        debug!("Buffered {}.", len);
                        return Ok(Some(len));
                    }
                    self.in_buffer = Cursor::new(new);
                }

                if let Some(next) = try!(self.socket.try_read_buf(self.in_buffer.get_mut())) {
                    if next == 0 {
                        return Ok(Some(len))
                    }
                    len += next
                } else {
                    debug!("Buffered {}.", len);
                    return Ok(Some(len))
                }
            }
        } else {
            Ok(None)
        }
    }
}
