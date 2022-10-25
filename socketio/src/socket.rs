use crate::error::{Error, Result};
use crate::packet::{Packet, PacketId};
use bytes::Bytes;
use rust_engineio::{Client as EngineClient, Packet as EnginePacket, PacketId as EnginePacketId};
use std::convert::TryFrom;
use std::sync::{atomic::AtomicBool, Arc};
use std::{fmt::Debug, sync::atomic::Ordering};

use super::{event::Event, payload::Payload};

/// Handles communication in the `socket.io` protocol.
#[derive(Clone, Debug)]
pub(crate) struct Socket {
    //TODO: 0.4.0 refactor this
    engine_client: Arc<EngineClient>,
    connected: Arc<AtomicBool>,
}

impl Socket {
    /// Creates an instance of `Socket`.

    pub(super) fn new(engine_client: EngineClient) -> Result<Self> {
        Ok(Socket {
            engine_client: Arc::new(engine_client),
            connected: Arc::new(AtomicBool::default()),
        })
    }

    /// Connects to the server. This includes a connection of the underlying
    /// engine.io client and afterwards an opening socket.io request.
    pub fn connect(&self) -> Result<()> {
        self.engine_client.connect()?;

        // store the connected value as true, if the connection process fails
        // later, the value will be updated
        self.connected.store(true, Ordering::Release);

        Ok(())
    }

    /// Disconnects from the server by sending a socket.io `Disconnect` packet. This results
    /// in the underlying engine.io transport to get closed as well.
    pub fn disconnect(&self) -> Result<()> {
        if self.is_engineio_connected()? {
            self.engine_client.disconnect()?;
        }
        if self.connected.load(Ordering::Acquire) {
            self.connected.store(false, Ordering::Release);
        }
        Ok(())
    }

    /// Sends a `socket.io` packet to the server using the `engine.io` client.
    pub fn send(&self, packet: Packet) -> Result<()> {
        if !self.is_engineio_connected()? || !self.connected.load(Ordering::Acquire) {
            return Err(Error::IllegalActionBeforeOpen());
        }

        // the packet, encoded as an engine.io message packet
        let engine_packet = EnginePacket::new(EnginePacketId::Message, Bytes::from(&packet));
        self.engine_client.emit(engine_packet)?;

        if let Some(attachments) = packet.attachments {
            for attachment in attachments {
                let engine_packet = EnginePacket::new(EnginePacketId::MessageBinary, attachment);
                self.engine_client.emit(engine_packet)?;
            }
        }

        Ok(())
    }

    /// Emits to certain event with given data.
    pub fn emit(&self, nsp: &str, event: Event, data: Payload) -> Result<()> {
        self.emit_multi(nsp, event, vec![data])
    }

    /// Emits to certain event with given vector of data.
    pub fn emit_multi(&self, nsp: &str, event: Event, data: Vec<Payload>) -> Result<()> {
        let socket_packet = Self::build_packet_for_payloads(data, Some(event), nsp, None, false)?;

        self.send(socket_packet)
    }

    /// Returns a packet for a payload, could be used for bot binary and non binary
    /// events and acks. Convenance method.
    #[inline]
    pub(crate) fn build_packet_for_payloads(
        payloads: Vec<Payload>,
        event: Option<Event>,
        nsp: &str,
        id: Option<i32>,
        is_ack: bool,
    ) -> Result<Packet> {
        let (data, attachments) = Self::encode_data(event, payloads);

        let packet_type = match attachments.is_empty() {
            true if is_ack => PacketId::Ack,
            true => PacketId::Event,
            false if is_ack => PacketId::BinaryAck,
            false => PacketId::BinaryEvent,
        };

        Ok(Packet::new(
            packet_type,
            nsp.to_owned(),
            Some(data),
            id,
            attachments.len() as u8,
            Some(attachments),
        ))
    }

    fn encode_data(event: Option<Event>, payloads: Vec<Payload>) -> (String, Vec<Bytes>) {
        let mut attachments = vec![];
        let mut data = "[".to_owned();

        if let Some(event) = event {
            data += &format!("\"{}\"", String::from(event));
            if !payloads.is_empty() {
                data += ","
            }
        }

        Self::encode_payloads(&mut data, payloads, &mut attachments);

        data += "]";

        (data, attachments)
    }

    fn encode_payloads(data: &mut String, payloads: Vec<Payload>, attachments: &mut Vec<Bytes>) {
        for (index, payload) in payloads.iter().enumerate() {
            match payload {
                Payload::Number(num) => *data += &format!("{}", num),
                Payload::Binary(bin_data) => {
                    *data += "{\"_placeholder\":true,\"num\":";
                    *data += &format!("{}", attachments.len());
                    *data += "}";
                    attachments.push(bin_data.to_owned());
                }
                Payload::String(str_data) => {
                    if serde_json::from_str::<serde_json::Value>(str_data).is_ok() {
                        *data += str_data
                    } else {
                        *data += &format!("\"{}\"", str_data)
                    };
                }
            };

            if index < payloads.len() - 1 {
                *data += ",";
            }
        }
    }

    pub(crate) fn poll(&self) -> Result<Option<Packet>> {
        loop {
            match self.engine_client.poll() {
                Ok(Some(packet)) => {
                    if packet.packet_id == EnginePacketId::Message
                        || packet.packet_id == EnginePacketId::MessageBinary
                    {
                        let packet = self.handle_engineio_packet(packet)?;
                        self.handle_socketio_packet(&packet);
                        return Ok(Some(packet));
                    } else {
                        continue;
                    }
                }
                Ok(None) => {
                    return Ok(None);
                }
                Err(err) => return Err(err.into()),
            }
        }
    }

    /// Handles the connection/disconnection.
    #[inline]
    fn handle_socketio_packet(&self, socket_packet: &Packet) {
        match socket_packet.packet_type {
            PacketId::Connect => {
                self.connected.store(true, Ordering::Release);
            }
            PacketId::ConnectError => {
                self.connected.store(false, Ordering::Release);
            }
            PacketId::Disconnect => {
                self.connected.store(false, Ordering::Release);
            }
            _ => (),
        }
    }

    /// Handles new incoming engineio packets
    fn handle_engineio_packet(&self, packet: EnginePacket) -> Result<Packet> {
        let mut socket_packet = Packet::try_from(&packet.data)?;

        // Only handle attachments if there are any
        if socket_packet.attachment_count > 0 {
            let mut attachments_left = socket_packet.attachment_count;
            let mut attachments = Vec::new();
            while attachments_left > 0 {
                let next = self.engine_client.poll();
                match next {
                    Err(err) => return Err(err.into()),
                    Ok(Some(packet)) => match packet.packet_id {
                        EnginePacketId::MessageBinary | EnginePacketId::Message => {
                            attachments.push(packet.data);
                            attachments_left -= 1;
                        }
                        _ => {
                            return Err(Error::InvalidAttachmentPacketType(
                                packet.packet_id.into(),
                            ));
                        }
                    },
                    Ok(None) => {
                        // Engineio closed before attachments completed.
                        return Err(Error::IncompletePacket());
                    }
                }
            }
            socket_packet.attachments = Some(attachments);
        }

        Ok(socket_packet)
    }

    fn is_engineio_connected(&self) -> Result<bool> {
        Ok(self.engine_client.is_connected()?)
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_build_multiple_payloads_packet() {
        let packet = Socket::build_packet_for_payloads(
            vec![crate::Payload::Binary(Bytes::from_static(&[1, 2, 3]))],
            Some("hello".into()),
            "/",
            None,
            false,
        );

        assert_eq!(
            Bytes::from(&packet.unwrap()),
            "51-[\"hello\",{\"_placeholder\":true,\"num\":0}]"
                .to_string()
                .into_bytes()
        );

        let packet = Socket::build_packet_for_payloads(
            vec![crate::Payload::Binary(Bytes::from_static(&[1, 2, 3]))],
            Some("project:delete".into()),
            "/admin",
            Some(456),
            false,
        );

        assert_eq!(
            Bytes::from(&packet.unwrap()),
            "51-/admin,456[\"project:delete\",{\"_placeholder\":true,\"num\":0}]"
                .to_string()
                .into_bytes()
        );

        let packet = Socket::build_packet_for_payloads(
            vec![crate::Payload::Binary(Bytes::from_static(&[3, 2, 1]))],
            None,
            "/admin",
            Some(456),
            true,
        );

        assert_eq!(
            Bytes::from(&packet.unwrap()),
            "61-/admin,456[{\"_placeholder\":true,\"num\":0}]"
                .to_string()
                .into_bytes()
        );

        let packet =
            Socket::build_packet_for_payloads(vec![], Some("hello".into()), "/", None, false);
        assert_eq!(
            Bytes::from(&packet.unwrap()),
            "2[\"hello\"]".to_string().into_bytes()
        );

        let payloads = vec![
            crate::Payload::Binary(Bytes::from_static(&[1, 2, 3])),
            "1".into(),
        ];
        let packet =
            Socket::build_packet_for_payloads(payloads, Some("hello".into()), "/", None, false);

        assert_eq!(
            Bytes::from(&packet.unwrap()),
            "51-[\"hello\",{\"_placeholder\":true,\"num\":0},1]"
                .to_string()
                .into_bytes()
        );

        let payloads = vec![
            crate::Payload::Binary(Bytes::from_static(&[1, 2, 3])),
            crate::Payload::Binary(Bytes::from_static(&[1, 2, 3])),
        ];
        let packet = Socket::build_packet_for_payloads(
            payloads,
            Some("project:delete".into()),
            "/admin",
            Some(456),
            false,
        );

        assert_eq!(
            Bytes::from(&packet.unwrap()),
            "52-/admin,456[\"project:delete\",{\"_placeholder\":true,\"num\":0},{\"_placeholder\":true,\"num\":1}]"
                .to_string()
                .into_bytes()
        );

        let payloads = vec![
            crate::Payload::Number(4),
            crate::Payload::Binary(Bytes::from_static(&[3, 2, 1])),
        ];
        let packet = Socket::build_packet_for_payloads(payloads, None, "/admin", Some(456), true);

        assert_eq!(
            Bytes::from(&packet.unwrap()),
            "61-/admin,456[4,{\"_placeholder\":true,\"num\":0}]"
                .to_string()
                .into_bytes()
        );
    }
}
