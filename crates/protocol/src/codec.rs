//! Framing: u32 LE length prefix + bincode body.
use crate::Message;
use std::io::{self, Read, Write};

const MAX_FRAME: u32 = 1 << 20;

pub fn write_msg<W: Write>(w: &mut W, msg: &Message) -> io::Result<()> {
    let body = bincode::serialize(msg).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    w.write_all(&(body.len() as u32).to_le_bytes())?;
    w.write_all(&body)?;
    w.flush()
}

pub fn read_msg<R: Read>(r: &mut R) -> io::Result<Message> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len);
    if len > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
    }
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf)?;
    bincode::deserialize(&buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::InputEvent;

    #[test]
    fn roundtrip() {
        let mut buf = Vec::new();
        write_msg(&mut buf, &Message::Input(InputEvent::MouseMoveRel { dx: 5, dy: -3 })).unwrap();
        match read_msg(&mut buf.as_slice()).unwrap() {
            Message::Input(InputEvent::MouseMoveRel { dx: 5, dy: -3 }) => {}
            m => panic!("unexpected {m:?}"),
        }
    }
}
