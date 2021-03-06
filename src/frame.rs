use std::fmt;
use std::mem::transmute;
use std::io::{Cursor, Read, Write};
use std::default::Default;
use std::iter::FromIterator;

use rand;
use mio::TryRead;

use result::{Result, Error, Kind};
use protocol::{OpCode, CloseCode};

fn apply_mask(buf: &mut [u8], mask: &[u8; 4]) {
    let iter = buf.iter_mut().zip(mask.iter().cycle());
    for (byte, &key) in iter {
        *byte ^= key
    }
}

#[inline]
fn generate_mask() -> [u8; 4] {
    unsafe { transmute(rand::random::<u32>()) }
}

/// A struct representing a WebSocket frame.
#[derive(Debug, Clone)]
pub struct Frame {
    finished: bool,
    rsv1: bool,
    rsv2: bool,
    rsv3: bool,
    opcode: OpCode,

    mask: Option<[u8; 4]>,

    payload: Vec<u8>,
}

impl Frame {

    /// Get the length of the frame.
    /// This is the length of the header + the length of the payload.
    #[inline]
    pub fn len(&self) -> usize {
        let mut header_length = 2;
        let payload_len = self.payload().len();
        if payload_len > 125 {
            if payload_len <= u16::max_value() as usize {
                header_length += 2;
            } else {
                header_length += 8;
            }
        }

        if self.is_masked() {
            header_length += 4;
        }

        header_length + payload_len
    }

    /// Test whether the frame is a final frame.
    #[inline]
    pub fn is_final(&self) -> bool {
        self.finished
    }

    /// Test whether the first reserved bit is set.
    #[inline]
    pub fn has_rsv1(&self) -> bool {
        self.rsv1
    }

    /// Test whether the second reserved bit is set.
    #[inline]
    pub fn has_rsv2(&self) -> bool {
        self.rsv2
    }

    /// Test whether the third reserved bit is set.
    #[inline]
    pub fn has_rsv3(&self) -> bool {
        self.rsv3
    }

    /// Get the OpCode of the frame.
    #[inline]
    pub fn opcode(&self) -> OpCode {
        self.opcode
    }

    /// Get a reference to the frame's payload.
    #[inline]
    pub fn payload(&self) -> &Vec<u8> {
        &self.payload
    }

    // Test whether the frame is masked.
    #[doc(hidden)]
    #[inline]
    pub fn is_masked(&self) -> bool {
        self.mask.is_some()
    }

    // Get an optional reference to the frame's mask.
    #[doc(hidden)]
    #[allow(dead_code)]
    #[inline]
    pub fn mask(&self) -> Option<&[u8; 4]> {
        self.mask.as_ref()
    }

    /// Make this frame a final frame.
    #[allow(dead_code)]
    #[inline]
    pub fn set_final(&mut self, is_final: bool) -> &mut Frame {
        self.finished = is_final;
        self
    }

    /// Set the first reserved bit.
    #[allow(dead_code)]
    #[inline]
    pub fn set_rsv1(&mut self, has_rsv1: bool) -> &mut Frame {
        self.rsv1 = has_rsv1;
        self
    }

    /// Set the second reserved bit.
    #[allow(dead_code)]
    #[inline]
    pub fn set_rsv2(&mut self, has_rsv2: bool) -> &mut Frame {
        self.rsv2 = has_rsv2;
        self
    }

    /// Set the third reserved bit.
    #[allow(dead_code)]
    #[inline]
    pub fn set_rsv3(&mut self, has_rsv3: bool) -> &mut Frame {
        self.rsv1 = has_rsv3;
        self
    }

    /// Set the OpCode.
    #[allow(dead_code)]
    #[inline]
    pub fn set_opcode(&mut self, opcode: OpCode) -> &mut Frame {
        self.opcode = opcode;
        self
    }

    /// Edit the frame's payload.
    #[allow(dead_code)]
    #[inline]
    pub fn payload_mut(&mut self) -> &mut Vec<u8> {
        &mut self.payload
    }

    // Generate a new mask for this frame.
    //
    // This method simply generates and stores the mask. It does not change the payload data.
    // Instead, the payload data will be masked with the generated mask when the frame is sent
    // to the other endpoint.
    #[doc(hidden)]
    #[inline]
    pub fn set_mask(&mut self) -> &mut Frame {
        self.mask = Some(generate_mask());
        self
    }

    // This method unmasks the payload and should only be called on frames that are actually
    // masked. In other words, those frames that have just been received from a client endpoint.
    #[doc(hidden)]
    #[inline]
    pub fn remove_mask(&mut self) -> &mut Frame {
        self.mask.and_then(|mask| {
            Some(apply_mask(&mut self.payload, &mask))
        });
        self.mask = None;
        self
    }

    /// Consume the frame into its payload.
    pub fn into_data(self) -> Vec<u8> {
        self.payload
    }

    /// Create a new data frame.
    #[inline]
    pub fn message(data: Vec<u8>, code: OpCode, finished: bool) -> Frame {
        debug_assert!(match code {
            OpCode::Text | OpCode::Binary | OpCode::Continue => true,
            _ => false,
        }, "Invalid opcode for data frame.");

        Frame {
            finished: finished,
            opcode: code,
            payload: data,
            .. Frame::default()
        }
    }

    /// Create a new Pong control frame.
    #[inline]
    pub fn pong(data: Vec<u8>) -> Frame {
        Frame {
            opcode: OpCode::Pong,
            payload: data,
            .. Frame::default()
        }
    }

    /// Create a new Ping control frame.
    #[inline]
    pub fn ping(data: Vec<u8>) -> Frame {
        Frame {
            opcode: OpCode::Ping,
            payload: data,
            .. Frame::default()
        }
    }

    /// Create a new Close control frame.
    #[inline]
    pub fn close(code: CloseCode, reason: &str) -> Frame {
        let raw: [u8; 2] = unsafe {
            let u: u16 = code.into();
            transmute(u.to_be())
        };

        let payload = if let CloseCode::Empty = code {
            Vec::new()
        } else {
            Vec::from_iter(
                raw[..].iter()
                       .chain(reason.as_bytes().iter())
                       .map(|&b| b))
        };

        Frame {
            payload: payload,
            .. Frame::default()
        }
    }

    /// Parse the input stream into a frame.
    pub fn parse(cursor: &mut Cursor<Vec<u8>>) -> Result<Option<Frame>> {
        let size = cursor.get_ref().len() - cursor.position() as usize;
        let initial = cursor.position();
        debug!("Position in buffer {}", initial);

        let mut head = [0u8; 2];
        if try!(cursor.read(&mut head)) != 2 {
            cursor.set_position(initial);
            return Ok(None)
        }

        debug!("Parsed headers {:?}", head);

        let first = head[0];
        let second = head[1];
        debug!("First: {:b}", first);
        debug!("Second: {:b}", second);

        let finished = first & 0x80 != 0;

        let rsv1 = first & 0x40 != 0;
        let rsv2 = first & 0x20 != 0;
        let rsv3 = first & 0x10 != 0;

        let opcode = OpCode::from(first & 0x0F);
        debug!("Opcode: {:?}", opcode);
        if let OpCode::Bad = opcode {
            return Err(Error::new(Kind::Protocol, format!("Encountered invalid opcode: {}", first & 0x0F)))
        }
        let masked = second & 0x80 != 0;
        debug!("Masked: {:?}", masked);

        let mut header_length = 2;

        let mut length = (second & 0x7F) as u64;

        if length == 126 {
            let mut length_bytes = [0u8; 2];
            if try!(cursor.read(&mut length_bytes)) != 2 {
                cursor.set_position(initial);
                return Ok(None)
            }

            length = unsafe {
                let mut wide: u16 = transmute(length_bytes);
                wide = u16::from_be(wide);
                wide
            } as u64;
            header_length += 2;
        } else if length == 127 {
            let mut length_bytes = [0u8; 8];
            if try!(cursor.read(&mut length_bytes)) != 8 {
                cursor.set_position(initial);
                return Ok(None)
            }

            unsafe { length = transmute(length_bytes); }
            length = u64::from_be(length);
            header_length += 8;
        }
        debug!("Payload length: {}", length);

        // control frames must have length <= 125
        match opcode {
            OpCode::Close | OpCode::Ping | OpCode::Pong if length > 125 => {
                return Err(Error::new(Kind::Protocol, format!("Rejected WebSocket handshake.Received control frame with length: {}.", length)))
            }
            _ => ()
        }

        let mask = if masked {
            let mut mask_bytes = [0u8; 4];
            if try!(cursor.read(&mut mask_bytes)) != 4 {
                cursor.set_position(initial);
                return Ok(None)
            } else {
                header_length += 4;
                Some(mask_bytes)
            }
        } else {
            None
        };

        if size < length as usize + header_length {
            cursor.set_position(initial);
            return Ok(None)
        }

        let mut data = Vec::with_capacity(length as usize);
        if length > 0 {
            // It is ok to unwrap here because this cursor won't block
            let read = try!(cursor.try_read_buf(&mut data)).unwrap();
            debug_assert!(read == length as usize, "Read incorrect payload length!");
        }

        let frame = Frame {
            finished: finished,
            rsv1: rsv1,
            rsv2: rsv2,
            rsv3: rsv3,
            opcode: opcode,
            mask: mask,
            payload: data,
        };
        Ok(Some(frame))
    }

    /// Write a frame out to a buffer
    pub fn format<W>(&mut self, w: &mut W) -> Result<()>
        where W: Write
    {
        let mut one = 0u8;
        let code: u8 = self.opcode.into();
        if self.is_final() {
            one |= 0x80;
        }
        if self.has_rsv1() {
            one |= 0x40;
        }
        if self.has_rsv2() {
            one |= 0x20;
        }
        if self.has_rsv3() {
            one |= 0x10;
        }
        one |= code;

        let mut two = 0u8;

        if self.is_masked() {
            two |= 0x80;
        }

        if self.payload.len() < 126 {
            two |= self.payload.len() as u8;
            let headers = [one, two];
            try!(w.write(&headers));
        } else if self.payload.len() <= 65535 {
            two |= 126;
            let length_bytes: [u8; 2] = unsafe {
                let short = self.payload.len() as u16;
                transmute(short.to_be())
            };
            let headers = [one, two, length_bytes[0], length_bytes[1]];
            try!(w.write(&headers));
        } else {
            two |= 127;
            let length_bytes: [u8; 8] = unsafe {
                transmute(self.payload.len().to_be())
            };
            let headers = [
                one,
                two,
                length_bytes[0],
                length_bytes[1],
                length_bytes[2],
                length_bytes[3],
                length_bytes[4],
                length_bytes[5],
                length_bytes[6],
                length_bytes[7],
            ];
            try!(w.write(&headers));
        }

        if self.is_masked() {
            let mask = self.mask.take().unwrap();
            apply_mask(&mut self.payload, &mask);
            try!(w.write(&mask));
        }

        try!(w.write(&self.payload));
        Ok(())
    }
}

impl Default for Frame {
    fn default() -> Frame {
        Frame {
            finished: true,
            rsv1: false,
            rsv2: false,
            rsv3: false,
            opcode: OpCode::Close,
            mask: None,
            payload: Vec::new(),
        }
    }
}

impl fmt::Display for Frame {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f,
            "
<FRAME>
final: {}
reserved: {} {} {}
opcode: {}
mask: {}
payload: {:?}
            ",
            self.finished,
            self.rsv1,
            self.rsv2,
            self.rsv3,
            self.opcode,
            self.mask.map(|mask| format!("{:?}", mask)).unwrap_or("NONE".into()),
            self.payload)
    }
}

mod test {
    #![allow(unused_imports, unused_variables, dead_code)]
    use super::*;
    use protocol::OpCode;

    #[test]
    fn test_display_frame() {
        let f = Frame::message("hi there".into(), OpCode::Text, true);
        let view = format!("{}", f);
        view.contains("payload:");
    }
}
