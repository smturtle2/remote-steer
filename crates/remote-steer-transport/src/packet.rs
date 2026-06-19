use remote_steer_core::{
    FfbCommand, FfbReply, RemoteSteerError, Result, WheelProfileId, WheelStateSnapshot,
};
use serde::{Deserialize, Serialize};

pub const MAGIC: [u8; 4] = *b"RSTR";
pub const PROTOCOL_VERSION: u16 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Channel {
    Session,
    Input,
    FfbControl,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Handshake {
    pub peer_name: String,
    pub profile: WheelProfileId,
    pub profile_hash: [u8; 32],
    pub max_effects: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TransportMessage {
    Hello(Handshake),
    HelloAck(Handshake),
    Input(WheelStateSnapshot),
    FfbCommand(FfbCommand),
    FfbReply(FfbReply),
    Ping { nonce: u64 },
    Pong { nonce: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WirePacket {
    pub magic: [u8; 4],
    pub version: u16,
    pub session_id: u64,
    pub seq: u64,
    pub channel: Channel,
    pub token_digest: [u8; 32],
    pub payload: Vec<u8>,
}

pub fn profile_hash(profile: WheelProfileId) -> [u8; 32] {
    let bytes = match profile {
        WheelProfileId::T150 => b"remote-steer-profile:t150:v1".as_slice(),
    };
    *blake3::hash(bytes).as_bytes()
}

pub fn encode_message(
    token: &str,
    session_id: u64,
    seq: u64,
    channel: Channel,
    message: &TransportMessage,
) -> Result<Vec<u8>> {
    let payload = bincode::serialize(message)
        .map_err(|err| RemoteSteerError::Serialization(err.to_string()))?;
    let token_digest = packet_digest(token, session_id, seq, channel, &payload);
    let packet = WirePacket {
        magic: MAGIC,
        version: PROTOCOL_VERSION,
        session_id,
        seq,
        channel,
        token_digest,
        payload,
    };
    bincode::serialize(&packet).map_err(|err| RemoteSteerError::Serialization(err.to_string()))
}

pub fn decode_message(token: &str, bytes: &[u8]) -> Result<(WirePacket, TransportMessage)> {
    let packet: WirePacket = bincode::deserialize(bytes)
        .map_err(|err| RemoteSteerError::InvalidPacket(err.to_string()))?;
    if packet.magic != MAGIC {
        return Err(RemoteSteerError::InvalidPacket("bad magic".to_string()));
    }
    if packet.version != PROTOCOL_VERSION {
        return Err(RemoteSteerError::InvalidPacket(format!(
            "unsupported protocol version {}",
            packet.version
        )));
    }
    let expected = packet_digest(
        token,
        packet.session_id,
        packet.seq,
        packet.channel,
        &packet.payload,
    );
    if expected != packet.token_digest {
        return Err(RemoteSteerError::AuthenticationFailed);
    }
    let message = bincode::deserialize(&packet.payload)
        .map_err(|err| RemoteSteerError::InvalidPacket(err.to_string()))?;
    Ok((packet, message))
}

fn packet_digest(
    token: &str,
    session_id: u64,
    seq: u64,
    channel: Channel,
    payload: &[u8],
) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"remote-steer-token-v1");
    hasher.update(token.as_bytes());
    hasher.update(&session_id.to_le_bytes());
    hasher.update(&seq.to_le_bytes());
    hasher.update(&[channel as u8]);
    hasher.update(payload);
    *hasher.finalize().as_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use remote_steer_core::WheelStateSnapshot;

    #[test]
    fn packet_round_trip_authenticates() {
        let message = TransportMessage::Input(WheelStateSnapshot::empty(7, 9));
        let bytes = encode_message("token", 1, 2, Channel::Input, &message).unwrap();
        let (_, decoded) = decode_message("token", &bytes).unwrap();
        assert_eq!(message, decoded);
        assert!(matches!(
            decode_message("wrong", &bytes),
            Err(RemoteSteerError::AuthenticationFailed)
        ));
    }
}
